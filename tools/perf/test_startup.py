#!/usr/bin/env python3
"""Regression tests for tools/perf/startup.py (stdlib unittest only).

Run from the repository root:
    python3 -m unittest discover -s tools/perf -p 'test_*.py'
or directly:
    python3 tools/perf/test_startup.py
"""

import argparse
import gc
import hashlib
import json
import math
import os
import shlex
import signal
import subprocess
import sys
import tempfile
import time
import unittest
import warnings

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import startup  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "startup.py")
POSIX_WAIT4 = hasattr(os, "wait4")
POSIX_SHELL = os.name == "posix"
BENIGN = [sys.executable, "-c", "pass"]
HOG = [sys.executable, "-c", "b = b'x' * (128 << 20)"]


class PercentileTests(unittest.TestCase):
    def test_empty_has_no_stats(self):
        self.assertTrue(math.isnan(startup.percentile([], 50)))
        self.assertEqual(startup.summarize([]),
                         {"n": 0, "p50_ms": None, "p95_ms": None, "max_ms": None})

    def test_single_sample(self):
        self.assertEqual(startup.percentile([7.0], 95), 7.0)

    def test_nearest_rank(self):
        self.assertEqual(startup.percentile([1, 2, 3, 4], 50), 2)
        self.assertEqual(startup.percentile([1, 2], 95), 2)
        self.assertEqual(startup.percentile(list(range(1, 101)), 95), 95)
        self.assertEqual(startup.percentile([10, 20, 30], 100), 30)

    def test_summarize(self):
        stats = startup.summarize([3.0, 1.0, 2.0])
        self.assertEqual(stats["n"], 3)
        self.assertEqual(stats["p50_ms"], 2.0)
        self.assertEqual(stats["max_ms"], 3.0)


class TimeoutValidationTests(unittest.TestCase):
    def test_accepts_positive_finite(self):
        self.assertEqual(startup.positive_seconds("0.5"), 0.5)
        self.assertEqual(startup.positive_seconds("60"), 60.0)

    def test_rejects_nonpositive_and_nonfinite(self):
        for value in ("0", "-1", "nan", "inf", "-inf", "abc", ""):
            with self.subTest(value=value):
                with self.assertRaises(argparse.ArgumentTypeError):
                    startup.positive_seconds(value)


class PurgeTests(unittest.TestCase):
    @unittest.skipUnless(POSIX_SHELL, "purge command uses a POSIX shell")
    def test_purge_nonzero_invalidates_cold(self):
        with self.assertRaises(startup.PurgeError) as ctx:
            startup.run_batch(BENIGN, 1, timeout=10, purge_cmd="exit 7")
        exc = ctx.exception
        self.assertEqual(exc.reason, "nonzero")
        self.assertEqual(exc.exit_code, 7)
        self.assertEqual(exc.sample_index, 0)
        self.assertEqual(exc.samples, [])

    @unittest.skipUnless(POSIX_SHELL, "purge command uses a POSIX shell")
    def test_purge_timeout_is_bounded_and_invalidates_cold(self):
        started = time.monotonic()
        with self.assertRaises(startup.PurgeError) as ctx:
            startup.run_batch(BENIGN, 1, timeout=0.3, purge_cmd="sleep 30")
        self.assertEqual(ctx.exception.reason, "timeout")
        self.assertIn("termination requested", ctx.exception.detail)
        self.assertNotIn("killed", ctx.exception.detail)
        self.assertLess(time.monotonic() - started, 8.0)

    @unittest.skipUnless(POSIX_SHELL, "purge command uses a POSIX shell")
    def test_purge_failure_keeps_partial_samples_as_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            marker = os.path.join(tmp, "purged-once")
            cmd = (f"if [ -e {shlex.quote(marker)} ]; then exit 9; "
                   f"else : > {shlex.quote(marker)}; fi")
            with self.assertRaises(startup.PurgeError) as ctx:
                startup.run_batch(BENIGN, 3, timeout=10, purge_cmd=cmd)
        exc = ctx.exception
        self.assertEqual(exc.reason, "nonzero")
        self.assertEqual(exc.exit_code, 9)
        self.assertEqual(exc.sample_index, 1)
        self.assertEqual(len(exc.samples), 1)
        self.assertEqual(exc.samples[0]["outcome"], "ok")


class SampleOutcomeTests(unittest.TestCase):
    def test_launch_error_is_a_sample_not_an_exception(self):
        samples = startup.run_batch(["/nonexistent/nexus-perf-target"], 2, timeout=5)
        self.assertEqual(len(samples), 2)
        for sample in samples:
            self.assertEqual(sample["outcome"], "launch_error")
            self.assertIn("FileNotFoundError", sample["error"])
            self.assertIsNone(sample["exit_code"])

    def test_nonzero_exit_retains_code_and_is_excluded(self):
        samples = startup.run_batch(
            [sys.executable, "-c", "raise SystemExit(3)"], 1, timeout=20
        )
        sample = samples[0]
        self.assertEqual(sample["outcome"], "nonzero")
        self.assertEqual(sample["exit_code"], 3)
        stats = startup.summarize_samples(samples)
        self.assertEqual(stats["successful"], 0)
        self.assertEqual(stats["invalid"], {"nonzero": 1})
        self.assertIsNone(stats["p50_ms"])

    def test_timeout_samples_are_bounded_and_invalid(self):
        started = time.monotonic()
        samples = startup.run_batch(
            [sys.executable, "-c", "import time; time.sleep(60)"], 2, timeout=0.4
        )
        wall = time.monotonic() - started
        self.assertEqual([s["outcome"] for s in samples], ["timeout", "timeout"])
        self.assertLess(wall, 10.0)
        if os.name == "posix":
            self.assertEqual(samples[0]["signal"], 9)  # SIGKILL after timeout
        stats = startup.summarize_samples(samples)
        self.assertEqual(stats["successful"], 0)
        self.assertEqual(stats["invalid"], {"timeout": 2})

    def test_watchdog_timeout_leaks_no_resource_warnings(self):
        with warnings.catch_warnings():
            warnings.simplefilter("error", ResourceWarning)
            sample = startup.measure_once(
                [sys.executable, "-c", "import time; time.sleep(30)"], timeout=0.3
            )
            gc.collect()  # force Popen.__del__ while warnings are errors
        self.assertEqual(sample["outcome"], "timeout")

    def test_stats_use_successful_samples_only(self):
        samples = [
            {"index": 0, "outcome": "ok", "elapsed_ms": 1.0, "max_rss": None,
             "exit_code": 0, "signal": None, "error": None},
            {"index": 1, "outcome": "nonzero", "elapsed_ms": 2.0, "max_rss": None,
             "exit_code": 4, "signal": None, "error": None},
            {"index": 2, "outcome": "timeout", "elapsed_ms": 500.0, "max_rss": None,
             "exit_code": None, "signal": 9, "error": "killed"},
            {"index": 3, "outcome": "launch_error", "elapsed_ms": 0.1, "max_rss": None,
             "exit_code": None, "signal": None, "error": "boom"},
            {"index": 4, "outcome": "ok", "elapsed_ms": 3.0, "max_rss": 100,
             "exit_code": 0, "signal": None, "error": None},
        ]
        stats = startup.summarize_samples(samples)
        self.assertEqual(stats["n"], 2)
        self.assertEqual(stats["p50_ms"], 1.0)
        self.assertEqual(stats["max_ms"], 3.0)
        self.assertEqual(stats["attempted"], 5)
        self.assertEqual(stats["successful"], 2)
        self.assertEqual(stats["invalid"],
                         {"nonzero": 1, "timeout": 1, "launch_error": 1})
        self.assertEqual(stats["max_rss"], 100)


@unittest.skipUnless(POSIX_SHELL, "mixed-outcome target uses a POSIX shell")
class MixedOutcomeExitTests(unittest.TestCase):
    def test_mixed_nonzero_and_success_exits_nonzero_with_stats_and_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            marker = shlex.quote(os.path.join(tmp, "alternate"))
            script = (f"if [ -e {marker} ]; then rm -f {marker}; exit 0; "
                      f"else : > {marker}; exit 7; fi")
            out = os.path.join(tmp, "perf.json")
            proc = subprocess.run(
                [sys.executable, SCRIPT, "/bin/sh", "-n", "4", "--json", out, "--", "-c", script],
                capture_output=True, text=True, timeout=120,
            )
            self.assertEqual(proc.returncode, 1, proc.stderr)
            with open(out, encoding="utf-8") as fh:
                entry = json.load(fh)["modes"][0]
            # Successful stats are kept and failed raw samples keep evidence.
            self.assertEqual([s["outcome"] for s in entry["samples"]],
                             ["nonzero", "ok", "nonzero", "ok"])
            self.assertEqual(entry["results"]["successful"], 2)
            self.assertEqual(entry["results"]["invalid"], {"nonzero": 2})
            self.assertIsNotNone(entry["results"]["p50_ms"])
            self.assertEqual(entry["samples"][0]["exit_code"], 7)

    def test_timeout_plus_success_still_exits_nonzero(self):
        with tempfile.TemporaryDirectory() as tmp:
            marker = shlex.quote(os.path.join(tmp, "first-ok"))
            script = (f"if [ -e {marker} ]; then sleep 60; "
                      f"else : > {marker}; exit 0; fi")
            proc = subprocess.run(
                [sys.executable, SCRIPT, "/bin/sh", "-n", "2", "--timeout", "0.5",
                 "--", "-c", script],
                capture_output=True, text=True, timeout=120,
            )
            self.assertEqual(proc.returncode, 1, proc.stderr)
            self.assertIn("successful=1/2", proc.stdout)
            self.assertIn("invalid=[timeout=1]", proc.stdout)


@unittest.skipUnless(POSIX_WAIT4 and POSIX_SHELL, "process-group cleanup requires POSIX")
class ProcessGroupCleanupTests(unittest.TestCase):
    def test_timeout_kills_target_process_group_grandchild(self):
        with tempfile.TemporaryDirectory() as tmp:
            pidfile = os.path.join(tmp, "grandchild.pid")
            script = (
                "import subprocess, sys, time\n"
                "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])\n"
                "with open(sys.argv[1], 'w') as fh:\n"
                "    fh.write(str(child.pid))\n"
                "time.sleep(60)\n"
            )
            sample = startup.measure_once([sys.executable, "-c", script, pidfile], timeout=1.0)
            self.assertEqual(sample["outcome"], "timeout", sample)
            deadline = time.monotonic() + 5.0
            pid = None
            while time.monotonic() < deadline:
                try:
                    with open(pidfile, encoding="utf-8") as fh:
                        pid = int(fh.read().strip())
                    break
                except (OSError, ValueError):
                    time.sleep(0.05)
            self.assertIsNotNone(pid, "target never recorded its grandchild pid")
            try:
                alive = True
                while time.monotonic() < deadline:
                    try:
                        os.kill(pid, 0)
                    except ProcessLookupError:
                        alive = False
                        break
                    except PermissionError:
                        pass
                    time.sleep(0.05)
                self.assertFalse(alive, "grandchild survived the target's group kill")
            finally:
                try:
                    os.kill(pid, signal.SIGKILL)
                except OSError:
                    pass


class Sha256Tests(unittest.TestCase):
    def test_sha256_matches_hashlib_and_skips_large_files(self):
        payload = b"nexus perf provenance"
        with tempfile.NamedTemporaryFile(delete=False) as fh:
            fh.write(payload)
            path = fh.name
        try:
            self.assertEqual(startup._sha256_file(path), hashlib.sha256(payload).hexdigest())
            self.assertTrue(startup._sha256_file(path, limit_bytes=1).startswith("skipped"))
            missing = os.path.join(tempfile.gettempdir(), "nexus-perf-missing")
            self.assertEqual(startup._sha256_file(missing), "unknown")
        finally:
            os.unlink(path)


@unittest.skipUnless(POSIX_WAIT4, "target-specific RSS requires POSIX os.wait4")
class TargetRssTests(unittest.TestCase):
    def test_target_rss_not_contaminated_by_prior_children(self):
        hog = startup.measure_once(HOG, timeout=30)
        small = startup.measure_once(BENIGN, timeout=30)
        self.assertEqual(hog["outcome"], "ok", hog)
        self.assertEqual(small["outcome"], "ok", small)
        self.assertIsNotNone(hog["max_rss"])
        self.assertIsNotNone(small["max_rss"])
        # A cumulative RUSAGE_CHILDREN high-water mark would report the hog's
        # RSS for the following lightweight target; os.wait4 must not.
        self.assertGreater(hog["max_rss"], small["max_rss"])

    @unittest.skipUnless(POSIX_SHELL, "purge command uses a POSIX shell")
    def test_purge_helper_rss_not_attributed_to_target(self):
        hog = startup.measure_once(HOG, timeout=30)
        self.assertEqual(hog["outcome"], "ok", hog)
        purge_cmd = f"{shlex.quote(sys.executable)} -c \"b = b'x' * (128 << 20)\""
        samples = startup.run_batch(BENIGN, 1, timeout=30, purge_cmd=purge_cmd)
        self.assertEqual(samples[0]["outcome"], "ok", samples[0])
        self.assertLess(samples[0]["max_rss"], hog["max_rss"] // 2)


class CliJsonTests(unittest.TestCase):
    def _run_cli(self, *args, timeout=120):
        return subprocess.run(
            [sys.executable, SCRIPT, *args],
            capture_output=True, text=True, timeout=timeout,
        )

    def test_json_record_cold_purge_success_and_legacy_flags(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self._run_cli(
                sys.executable, "-n", "2", "--mode", "cold", "--purge-cmd", "true",
                "--timeout", "30", "--build-profile", "test/smoke",
                "--json", out, "--", "-c", "pass",
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            with open(out, encoding="utf-8") as fh:
                text = fh.read()
            self.assertNotIn("NaN", text)
            record = json.loads(text)
            self.assertEqual(record["schema"], "nexus.perf.startup/1")
            self.assertEqual(record["methodology"]["build_profile"], "test/smoke")
            self.assertEqual(record["methodology"]["runs_per_mode"], 2)
            self.assertEqual(len(record["modes"]), 1)
            entry = record["modes"][0]
            self.assertEqual(entry["mode"], "cold")
            self.assertTrue(entry["valid"])
            self.assertEqual(entry["purge_runs"], 2)
            self.assertEqual(entry["results"]["successful"], 2)
            self.assertEqual(entry["results"]["invalid"], {})
            self.assertEqual(len(entry["samples"]), 2)
            # Purge success is labeled as unverified cache prep, never cold.
            self.assertIn("cache reset not verified", entry["cache_prep"])
            self.assertFalse(record["methodology"]["cache_reset_verified"])
            self.assertIn("cannot verify", proc.stdout)
            self.assertEqual(len(record["methodology"]["binary_sha256"]), 64)

    @unittest.skipUnless(POSIX_SHELL, "purge command uses a POSIX shell")
    def test_cold_purge_failure_exits_2_and_marks_mode_invalid(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self._run_cli(
                sys.executable, "-n", "1", "--mode", "cold", "--purge-cmd", "exit 9",
                "--json", out, "--", "-c", "pass",
            )
            self.assertEqual(proc.returncode, 2, proc.stderr)
            with open(out, encoding="utf-8") as fh:
                record = json.load(fh)
            entry = record["modes"][0]
            self.assertFalse(entry["valid"])
            self.assertIn("nonzero", entry["invalid_reason"])
            self.assertEqual(entry["purge_error"]["exit_code"], 9)
            self.assertEqual(entry["purge_runs"], 1)
            self.assertEqual(entry["cache_prep"], "purge failed; cache reset not verified")
            self.assertIsNone(entry["results"])

    @unittest.skipUnless(POSIX_SHELL, "purge command uses a POSIX shell")
    def test_warm_mode_records_configured_purge_as_not_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self._run_cli(
                sys.executable, "-n", "1", "--mode", "warm", "--purge-cmd", "exit 9",
                "--json", out, "--", "-c", "pass",
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            with open(out, encoding="utf-8") as fh:
                record = json.load(fh)
            self.assertIn("configured for cold mode", record["methodology"]["cold_cache_prep"])
            entry = record["modes"][0]
            self.assertEqual(entry["purge_runs"], 0)
            self.assertEqual(entry["cache_prep"], "none")

    def test_all_launch_errors_exit_1_with_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self._run_cli(
                "/nonexistent/nexus-perf-target", "-n", "1", "--json", out,
            )
            self.assertEqual(proc.returncode, 1, proc.stderr)
            with open(out, encoding="utf-8") as fh:
                record = json.load(fh)
            entry = record["modes"][0]
            self.assertTrue(entry["valid"])
            self.assertEqual(entry["results"]["successful"], 0)
            self.assertEqual(entry["samples"][0]["outcome"], "launch_error")

    def test_cli_rejects_zero_timeout(self):
        proc = self._run_cli("/nonexistent/nexus-perf-target", "--timeout", "0")
        self.assertEqual(proc.returncode, 2)
        self.assertIn("positive finite", proc.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
