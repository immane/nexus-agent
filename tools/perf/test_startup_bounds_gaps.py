#!/usr/bin/env python3
"""Gap tests for measurement bounds in tools/perf/startup.py.

Complements test_startup.py, which covers the primary happy/failure paths.
This file pins the bounds the summaries depend on:

- a timed-out launch is bounded in wall time and in its own recorded duration,
  and keeps its evidence fields instead of collapsing to an exception;
- a launch failure is a sample with evidence, not a raised error, and never
  contributes to timing/RSS stats;
- timing and max RSS come from successful samples only, even when invalid
  samples carry large elapsed times or large RSS;
- every outcome the harness itself produces is accounted for exactly once in
  attempted/successful/invalid counts.

Deterministic and fast: no network, per-launch timeouts under a second, and no
assertion on absolute timings.

Run from the repository root:
    python3 -m unittest discover -s tools/perf -p 'test_*.py'
or directly:
    python3 tools/perf/test_startup_bounds_gaps.py
"""

import os
import stat
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import startup  # noqa: E402

POSIX = os.name == "posix"
SLEEPER = [sys.executable, "-c", "import time; time.sleep(60)"]
BENIGN = [sys.executable, "-c", "pass"]
MISSING = "/nonexistent/nexus-perf-bounds-target"
# A timeout must be long enough to exceed interpreter startup but short enough
# that a missing kill still finishes quickly.
SHORT_TIMEOUT_S = 0.5


@unittest.skipUnless(POSIX, "timeout signal evidence is POSIX-specific")
class TimeoutSampleBoundsTests(unittest.TestCase):
    def test_timeout_sample_duration_is_bounded_by_the_timeout(self):
        sample = startup.measure_once(SLEEPER, timeout=SHORT_TIMEOUT_S)
        self.assertEqual(sample["outcome"], "timeout", sample)
        # Measured from before the spawn, so at least the wait itself elapsed,
        # but nowhere near the target's own 60s lifetime.
        self.assertGreaterEqual(sample["elapsed_ms"], SHORT_TIMEOUT_S * 1000 * 0.5)
        self.assertLess(sample["elapsed_ms"], 5000.0)

    def test_timeout_sample_retains_evidence_fields(self):
        sample = startup.measure_once(SLEEPER, timeout=SHORT_TIMEOUT_S)
        self.assertEqual(sample["outcome"], "timeout", sample)
        self.assertIsNone(sample["exit_code"])
        self.assertEqual(sample["signal"], 9)  # SIGKILL after the timeout
        self.assertIn("exceeded timeout", sample["error"])
        self.assertIn("termination requested", sample["error"])
        self.assertIsNotNone(sample["max_rss"])

    def test_timeout_samples_excluded_from_timing_stats(self):
        timeout_sample = startup.measure_once(SLEEPER, timeout=SHORT_TIMEOUT_S)
        ok_sample = startup.measure_once(BENIGN, timeout=30)
        self.assertEqual(timeout_sample["outcome"], "timeout", timeout_sample)
        self.assertEqual(ok_sample["outcome"], "ok", ok_sample)

        stats = startup.summarize_samples([dict(timeout_sample, index=0), dict(ok_sample, index=1)])
        self.assertEqual(stats["n"], 1)
        self.assertEqual(stats["attempted"], 2)
        self.assertEqual(stats["successful"], 1)
        self.assertEqual(stats["invalid"], {"timeout": 1})
        self.assertEqual(stats["p50_ms"], ok_sample["elapsed_ms"])
        self.assertEqual(stats["max_ms"], ok_sample["elapsed_ms"])
        # The timed-out sample's own duration is larger than the reported max.
        self.assertGreater(timeout_sample["elapsed_ms"], stats["max_ms"])

    def test_timeout_rss_not_reported_as_successful_max_rss(self):
        # Allocates a large buffer, then exceeds the timeout: the sample keeps
        # its rusage RSS but must not count as a successful target.
        hog = [sys.executable, "-c", "b = b'x' * (48 << 20); import time; time.sleep(60)"]
        timed_out = startup.measure_once(hog, timeout=1.0)
        small = startup.measure_once(BENIGN, timeout=30)
        self.assertEqual(timed_out["outcome"], "timeout", timed_out)
        self.assertEqual(small["outcome"], "ok", small)
        self.assertIsNotNone(timed_out["max_rss"])
        self.assertIsNotNone(small["max_rss"])

        stats = startup.summarize_samples([dict(timed_out, index=0), dict(small, index=1)])
        self.assertEqual(stats["max_rss"], small["max_rss"])
        self.assertLess(small["max_rss"], timed_out["max_rss"])


class LaunchErrorSampleTests(unittest.TestCase):
    def test_launch_error_sample_shape_has_bounded_elapsed(self):
        sample = startup.measure_once([MISSING], timeout=30)
        self.assertEqual(sample["outcome"], "launch_error", sample)
        self.assertIn("FileNotFoundError", sample["error"])
        self.assertIsNone(sample["exit_code"])
        self.assertIsNone(sample["signal"])
        self.assertIsNone(sample["max_rss"])
        self.assertGreaterEqual(sample["elapsed_ms"], 0.0)
        self.assertLess(sample["elapsed_ms"], 1000.0)

    def test_launch_error_batch_reports_no_timing_or_rss_stats(self):
        samples = startup.run_batch([MISSING], 3, timeout=30)
        self.assertEqual([s["outcome"] for s in samples], ["launch_error"] * 3)
        self.assertEqual([s["index"] for s in samples], [0, 1, 2])

        stats = startup.summarize_samples(samples)
        self.assertEqual(stats["attempted"], 3)
        self.assertEqual(stats["successful"], 0)
        self.assertEqual(stats["invalid"], {"launch_error": 3})
        self.assertEqual(stats["n"], 0)
        self.assertIsNone(stats["p50_ms"])
        self.assertIsNone(stats["p95_ms"])
        self.assertIsNone(stats["max_ms"])
        self.assertIsNone(stats["max_rss"])

    @unittest.skipUnless(POSIX, "exec-bit check is POSIX-specific")
    def test_non_executable_existing_path_is_a_launch_error_sample(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "not-executable")
            with open(path, "w", encoding="utf-8") as fh:
                fh.write("#!/bin/sh\nexit 0\n")
            os.chmod(path, stat.S_IRUSR | stat.S_IWUSR)
            sample = startup.measure_once([path], timeout=30)
        self.assertEqual(sample["outcome"], "launch_error", sample)
        self.assertIsNotNone(sample["error"])
        self.assertIsNone(sample["exit_code"])
        self.assertEqual(startup.summarize_samples([sample])["invalid"], {"launch_error": 1})


@unittest.skipUnless(POSIX, "exit-code accounting uses POSIX wait statuses")
class MixedOutcomeAccountingTests(unittest.TestCase):
    def test_signal_terminated_target_is_nonzero_with_signal_evidence(self):
        sample = startup.measure_once(
            [sys.executable, "-c", "import os, signal; os.kill(os.getpid(), signal.SIGKILL)"],
            timeout=30,
        )
        self.assertEqual(sample["outcome"], "nonzero", sample)
        self.assertIsNone(sample["exit_code"])
        self.assertEqual(sample["signal"], 9)
        self.assertIn("terminated by signal 9", sample["error"])

        stats = startup.summarize_samples([sample])
        self.assertEqual(stats["successful"], 0)
        self.assertEqual(stats["invalid"], {"nonzero": 1})
        self.assertIsNone(stats["max_ms"])

    def test_all_harness_outcomes_accounted_exactly_once(self):
        samples = [
            startup.measure_once(BENIGN, timeout=30),
            startup.measure_once([sys.executable, "-c", "raise SystemExit(5)"], timeout=30),
            startup.measure_once(
                [sys.executable, "-c", "import os, signal; os.kill(os.getpid(), signal.SIGKILL)"],
                timeout=30,
            ),
            startup.measure_once([MISSING], timeout=30),
            startup.measure_once(SLEEPER, timeout=SHORT_TIMEOUT_S),
        ]
        stats = startup.summarize_samples(samples)
        self.assertEqual([s["outcome"] for s in samples],
                         ["ok", "nonzero", "nonzero", "launch_error", "timeout"])
        self.assertEqual(stats["attempted"], len(samples))
        self.assertEqual(stats["successful"], 1)
        self.assertEqual(stats["n"], 1)
        self.assertEqual(stats["invalid"],
                         {"nonzero": 2, "launch_error": 1, "timeout": 1})
        # attempted == successful + sum(invalid counts), for harness outcomes.
        self.assertEqual(stats["attempted"],
                         stats["successful"] + sum(stats["invalid"].values()))

    def test_empty_batch_summarizes_as_zero_attempts(self):
        self.assertEqual(startup.run_batch(BENIGN, 0, timeout=30), [])
        stats = startup.summarize_samples([])
        self.assertEqual(stats["attempted"], 0)
        self.assertEqual(stats["successful"], 0)
        self.assertEqual(stats["n"], 0)
        self.assertEqual(stats["invalid"], {})
        self.assertIsNone(stats["p50_ms"])
        self.assertIsNone(stats["max_rss"])


if __name__ == "__main__":
    unittest.main(verbosity=2)