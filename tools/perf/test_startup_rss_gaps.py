#!/usr/bin/env python3
"""Gap tests for per-target RSS attribution in tools/perf/startup.py.

Complements tools/perf/test_startup.py: this file targets the claims made in
the "Per-target RSS" section of tools/perf/README.md that the base suite does
not exercise directly, namely

- the reported number really is the exact waited child's rusage (magnitude and
  repeatability, not only ordering),
- purge helpers (including helpers with their own hogging descendants) never
  land in a target sample,
- the RSS summary is computed over successful samples only, including when the
  discarded samples carry large RSS evidence, and
- a timeout kills the spawned process group and reaps the direct child, leaving
  no zombie behind.

Run from the repository root:
    python3 -m unittest discover -s tools/perf -p 'test_*.py'
or directly:
    python3 tools/perf/test_startup_rss_gaps.py

stdlib unittest only; no network, no privileged or cache-purging commands.
"""

import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import startup  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "startup.py")
POSIX_WAIT4 = hasattr(os, "wait4")
POSIX_SHELL = os.name == "posix"
MAC_RSS = sys.platform == "darwin"
LINUX_RSS = sys.platform.startswith("linux")
PS = shutil.which("ps")
NEEDS_RSS = MAC_RSS or LINUX_RSS

MiB = 1 << 20

# A 128 MiB allocation lands well above LOWER_BOUND_RSS regardless of how much
# of it the allocator keeps resident, and stays far below UPPER_BOUND_RSS so a
# leaked helper or a stale high-water mark cannot hide inside the slack.
BIG_ALLOC = 128
LOWER_BOUND_RSS = 100 * MiB
UPPER_BOUND_RSS = 64 * MiB

BENIGN = [sys.executable, "-c", "pass"]
HOG = [sys.executable, "-c", f"b = b'x' * ({BIG_ALLOC} << 20)"]
NONZERO_HOG = [sys.executable, "-c",
               f"b = b'x' * ({BIG_ALLOC} << 20)\nraise SystemExit(3)"]
TIMEOUT_HOG = [sys.executable, "-c",
               f"import time\nb = b'x' * ({BIG_ALLOC} << 20)\ntime.sleep(60)"]
SLEEPER = [sys.executable, "-c", "import time; time.sleep(60)"]


def rss_bytes(raw):
    """Convert a ru_maxrss value to bytes using the documented unit."""
    if MAC_RSS:
        return raw
    return raw * 1024


def zombie_child_pids():
    """Pids of this process' children currently in the zombie state."""
    out = subprocess.run([PS, "-o", "pid=,ppid=,stat="],
                         capture_output=True, text=True, timeout=30).stdout
    mine = str(os.getpid())
    pids = []
    for line in out.splitlines():
        fields = line.split()
        if len(fields) == 3 and fields[1] == mine and fields[2].startswith("Z"):
            pids.append(int(fields[0]))
    return pids


def wait_gone(pid, timeout=10.0):
    """Poll until `pid` no longer exists; True if it disappeared in time."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return True
        except PermissionError:
            pass
        time.sleep(0.05)
    return False


def force_kill(pid):
    """Best-effort cleanup for a descendant that outlived its test."""
    try:
        os.kill(pid, 9)
    except OSError:
        pass


def _chain_argv(pidfile):
    """Target -> mid-process -> sleeper, all inside the spawned process group.

    Both levels are created without a new session, so both stay in the group
    that startup.py scopes its timeout kill to.
    """
    mid = (
        "import os, subprocess, sys, time\n"
        "sleeper = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])\n"
        "with open(sys.argv[1], 'w') as fh:\n"
        "    fh.write(str(os.getpid()) + ' ' + str(sleeper.pid) + '\\n')\n"
        "time.sleep(60)\n"
    )
    target = (
        "import subprocess, sys, time\n"
        f"subprocess.Popen([sys.executable, '-c', {mid!r}, sys.argv[1]])\n"
        "time.sleep(60)\n"
    )
    return [sys.executable, "-c", target, pidfile]


def _hog_purge_cmd(alloc_mb=64):
    """User-supplied cache prep that allocates, for purge-contamination tests."""
    script = f"b = b'x' * ({alloc_mb} << 20)"
    return f"{shlex.quote(sys.executable)} -c {shlex.quote(script)}"


def _nested_hog_purge_cmd(alloc_mb=128):
    """Cache prep whose own descendant allocates: the hardest case to leak."""
    inner = f"b = b'x' * ({alloc_mb} << 20)"
    parent = f"import subprocess, sys; subprocess.run([sys.executable, '-c', {inner!r}])"
    return f"{shlex.quote(sys.executable)} -c {shlex.quote(parent)}"


@unittest.skipUnless(POSIX_WAIT4 and NEEDS_RSS, "per-target RSS needs os.wait4 with a known unit")
class ExactChildRssTests(unittest.TestCase):
    def test_reported_rss_tracks_the_targets_own_allocation(self):
        """The value is a real measurement of this child, not a constant."""
        big = startup.measure_once(HOG, timeout=60)
        small = startup.measure_once(BENIGN, timeout=60)
        self.assertEqual(big["outcome"], "ok", big)
        self.assertEqual(small["outcome"], "ok", small)
        self.assertIsInstance(big["max_rss"], int)
        self.assertIsInstance(small["max_rss"], int)
        self.assertGreaterEqual(rss_bytes(big["max_rss"]), LOWER_BOUND_RSS,
                                "128 MiB target did not report a matching RSS")
        self.assertLess(rss_bytes(small["max_rss"]), UPPER_BOUND_RSS,
                        "baseline target reported an RSS no baseline process should reach")

    def test_repeated_launches_of_one_target_report_comparable_rss(self):
        """Each launch is attributed separately; a stale mark would drift."""
        first = startup.measure_once(BENIGN, timeout=60)
        second = startup.measure_once(BENIGN, timeout=60)
        self.assertEqual(first["outcome"], "ok", first)
        self.assertEqual(second["outcome"], "ok", second)
        for sample in (first, second):
            self.assertLess(rss_bytes(sample["max_rss"]), UPPER_BOUND_RSS, sample)
        self.assertLessEqual(rss_bytes(second["max_rss"]),
                             rss_bytes(first["max_rss"]) * 3 + UPPER_BOUND_RSS)

    def test_summary_max_rss_is_the_largest_successful_child_value(self):
        """Exactly the biggest child's own number: not summed, not accumulated."""
        hog = startup.measure_once(HOG, timeout=60)
        small = startup.measure_once(BENIGN, timeout=60)
        self.assertEqual(hog["outcome"], "ok", hog)
        self.assertEqual(small["outcome"], "ok", small)
        stats = startup.summarize_samples([dict(hog, index=0), dict(small, index=1)])
        self.assertEqual(stats["successful"], 2)
        self.assertEqual(stats["max_rss"], max(hog["max_rss"], small["max_rss"]))
        self.assertEqual(stats["max_rss"], hog["max_rss"])

    def test_rss_is_reported_unavailable_without_wait4_instead_of_fabricated(self):
        """Non-wait4 platforms keep timing and report no RSS at all."""
        original = startup.POSIX_WAIT4
        self.addCleanup(setattr, startup, "POSIX_WAIT4", original)
        startup.POSIX_WAIT4 = False
        try:
            self.assertIn("unavailable", startup.rss_method())
            sample = startup.measure_once(BENIGN, timeout=60)
        finally:
            startup.POSIX_WAIT4 = original
        self.assertEqual(sample["outcome"], "ok", sample)
        self.assertIsNone(sample["max_rss"])
        stats = startup.summarize_samples([dict(sample, index=0)])
        self.assertEqual(stats["successful"], 1)
        self.assertIsNone(stats["max_rss"])

    def test_recorded_rss_method_claims_exact_child_attribution(self):
        """The methodology string must not overstate what was measured."""
        method = startup.rss_method()
        self.assertIn("os.wait4", method)
        self.assertNotIn("unavailable", method)
        self.assertIn("directly waited", method)


@unittest.skipUnless(POSIX_WAIT4 and NEEDS_RSS and POSIX_SHELL,
                     "purge attribution needs a POSIX shell and os.wait4")
class PurgeHelperRssTests(unittest.TestCase):
    def test_hogging_purge_helper_before_every_sample_leaves_target_rss_small(self):
        cmd = _hog_purge_cmd()
        samples = startup.run_batch(BENIGN, 3, timeout=60, purge_cmd=cmd)
        self.assertEqual([s["outcome"] for s in samples], ["ok"] * 3, samples)
        for index, sample in enumerate(samples):
            self.assertLess(rss_bytes(sample["max_rss"]), UPPER_BOUND_RSS,
                            f"sample {index} absorbed the purge helper's RSS: {sample}")

    def test_purge_helper_with_its_own_hogging_descendant_is_not_attributed(self):
        samples = startup.run_batch(BENIGN, 1, timeout=60, purge_cmd=_nested_hog_purge_cmd())
        self.assertEqual(samples[0]["outcome"], "ok", samples[0])
        self.assertLess(rss_bytes(samples[0]["max_rss"]), UPPER_BOUND_RSS, samples[0])

    def test_purge_evidence_carries_no_rss_field(self):
        """Helper memory is never recorded, so it cannot be reported anywhere."""
        evidence = startup.run_purge(_hog_purge_cmd(), 60)
        self.assertEqual(sorted(evidence),
                         ["command", "elapsed_ms", "exit_code", "signal"])
        self.assertEqual(evidence["exit_code"], 0)

    def test_failed_purge_evidence_carries_no_rss_field(self):
        with self.assertRaises(startup.PurgeError) as ctx:
            startup.run_purge("exit 7", 30)
        exc = ctx.exception
        self.assertEqual(exc.reason, "nonzero")
        self.assertFalse(hasattr(exc, "rss"))
        for attr in ("command", "elapsed_ms", "exit_code", "signal", "samples",
                     "sample_index", "reason", "detail"):
            self.assertTrue(hasattr(exc, attr), attr)


class SuccessfulSampleRssSummaryTests(unittest.TestCase):
    @staticmethod
    def _sample(index, outcome, rss, exit_code=None, signum=None, error=None):
        return {"index": index, "outcome": outcome, "elapsed_ms": float(index + 1),
                "max_rss": rss, "exit_code": exit_code, "signal": signum, "error": error}

    def test_large_rss_on_invalid_samples_is_excluded_from_the_summary(self):
        samples = [
            self._sample(0, "ok", None, exit_code=0),
            self._sample(1, "nonzero", 900_000_000, exit_code=3),
            self._sample(2, "timeout", 800_000_000, signum=9),
            self._sample(3, "wait_error", 700_000_000, error="status collection failed"),
            self._sample(4, "launch_error", None, error="FileNotFoundError"),
        ]
        stats = startup.summarize_samples(samples)
        self.assertEqual(stats["successful"], 1)
        self.assertEqual(stats["invalid"],
                         {"nonzero": 1, "timeout": 1, "launch_error": 1, "wait_error": 1})
        self.assertIsNone(stats["max_rss"])

    def test_zero_rss_is_kept_and_missing_rss_is_not_read_as_zero(self):
        stats = startup.summarize_samples([
            self._sample(0, "ok", 0, exit_code=0),
            self._sample(1, "ok", None, exit_code=0),
        ])
        self.assertEqual(stats["max_rss"], 0)
        stats = startup.summarize_samples([
            self._sample(0, "ok", None, exit_code=0),
            self._sample(1, "ok", None, exit_code=0),
        ])
        self.assertIsNone(stats["max_rss"])

    @unittest.skipUnless(POSIX_WAIT4 and NEEDS_RSS,
                         "measured exclusion needs os.wait4 with a known unit")
    def test_measured_nonzero_and_timeout_targets_are_excluded_from_the_summary(self):
        failed = [
            startup.measure_once(NONZERO_HOG, timeout=60),
            startup.measure_once(TIMEOUT_HOG, timeout=1.0),
        ]
        good = startup.measure_once(BENIGN, timeout=60)
        self.assertEqual([s["outcome"] for s in failed], ["nonzero", "timeout"], failed)
        self.assertEqual(good["outcome"], "ok", good)
        # The discarded samples really did carry RSS evidence, so the summary
        # below is excluding measurements rather than ignoring absent values.
        for sample in failed:
            self.assertGreaterEqual(rss_bytes(sample["max_rss"]), LOWER_BOUND_RSS, sample)
        samples = [dict(failed[0], index=0), dict(failed[1], index=1), dict(good, index=2)]
        stats = startup.summarize_samples(samples)
        self.assertEqual(stats["successful"], 1)
        self.assertEqual(stats["max_rss"], good["max_rss"])
        self.assertLess(rss_bytes(stats["max_rss"]), UPPER_BOUND_RSS, stats)


@unittest.skipUnless(POSIX_WAIT4 and NEEDS_RSS,
                     "recorded RSS needs os.wait4 with a known unit")
class JsonRssReportingTests(unittest.TestCase):
    def _run_cli(self, *args, timeout=180):
        return subprocess.run([sys.executable, SCRIPT, *args],
                              capture_output=True, text=True, timeout=timeout)

    def test_json_record_keeps_per_sample_rss_with_exact_child_method(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self._run_cli(
                sys.executable, "-n", "2", "--timeout", "60", "--json", out, "--", "-c", "pass",
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            with open(out, encoding="utf-8") as fh:
                record = json.load(fh)
            self.assertEqual(record["methodology"]["rss_method"], startup.rss_method())
            entry = record["modes"][0]
            self.assertEqual(len(entry["samples"]), 2)
            for sample in entry["samples"]:
                self.assertIsInstance(sample["max_rss"], int)
                self.assertLess(rss_bytes(sample["max_rss"]), UPPER_BOUND_RSS, sample)
            self.assertEqual(entry["results"]["max_rss"],
                             max(s["max_rss"] for s in entry["samples"]))
            self.assertIn("max_rss_successful=", proc.stdout)
            self.assertIn(startup.rss_unit(), proc.stdout)

    def test_all_invalid_batch_records_no_successful_rss(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self._run_cli(
                sys.executable, "-n", "1", "--timeout", "60", "--json", out, "--",
                "-c", f"b = b'x' * ({BIG_ALLOC} << 20)\nraise SystemExit(3)",
            )
            self.assertEqual(proc.returncode, 1, proc.stderr)
            self.assertNotIn("max_rss_successful", proc.stdout)
            with open(out, encoding="utf-8") as fh:
                record = json.load(fh)
            entry = record["modes"][0]
            self.assertEqual(entry["results"]["successful"], 0)
            self.assertIsNone(entry["results"]["max_rss"])
            # Raw evidence is kept even though it must not be summarized.
            sample = entry["samples"][0]
            self.assertEqual(sample["outcome"], "nonzero")
            self.assertGreaterEqual(rss_bytes(sample["max_rss"]), LOWER_BOUND_RSS, sample)


@unittest.skipUnless(POSIX_WAIT4 and POSIX_SHELL,
                     "process-group cleanup requires POSIX")
class TimeoutReapAndZombieGapTests(unittest.TestCase):
    def test_timed_out_child_is_reaped_so_no_zombie_survives(self):
        # _wait_bounded is exercised directly here: it owns the reaping, and
        # only the caller-held Popen makes the already-reaped state observable.
        proc = subprocess.Popen(SLEEPER, stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL, start_new_session=True)
        waited = startup._wait_bounded(proc, 0.3, kill_group=True)
        self.assertTrue(waited["timed_out"], waited)
        self.assertEqual(waited["signal"], 9, waited)
        self.assertIsNone(waited["error"], waited)
        self.assertIsNotNone(proc.returncode)
        # An already-reaped child cannot be waited on again. An unreaped zombie
        # would still answer this waitpid with (0, WNOHANG) instead of raising.
        with self.assertRaises(ChildProcessError):
            os.waitpid(proc.pid, os.WNOHANG)

    def test_timeout_of_a_hogging_target_keeps_rss_evidence_but_no_reap_error(self):
        sample = startup.measure_once(TIMEOUT_HOG, timeout=1.0)
        self.assertEqual(sample["outcome"], "timeout", sample)
        self.assertEqual(sample["signal"], 9, sample)
        self.assertIn("termination requested", sample["error"])
        self.assertNotIn("not reaped", sample["error"])

    @unittest.skipUnless(PS, "zombie-state inspection needs ps(1)")
    def test_repeated_timeouts_and_a_purge_timeout_leave_no_zombie_children(self):
        samples = startup.run_batch(SLEEPER, 3, timeout=0.3)
        self.assertEqual([s["outcome"] for s in samples], ["timeout"] * 3, samples)
        with self.assertRaises(startup.PurgeError) as ctx:
            startup.run_purge("sleep 30", 0.3)
        self.assertEqual(ctx.exception.reason, "timeout")
        self.assertEqual(zombie_child_pids(), [])

    def test_timeout_kills_multi_level_descendants_in_the_spawned_group(self):
        with tempfile.TemporaryDirectory() as tmp:
            pidfile = os.path.join(tmp, "chain.pid")
            sample = startup.measure_once(_chain_argv(pidfile), timeout=2.0)
            self.assertEqual(sample["outcome"], "timeout", sample)
            pids = []
            deadline = time.monotonic() + 10.0
            while time.monotonic() < deadline and not pids:
                try:
                    with open(pidfile, encoding="utf-8") as fh:
                        pids = [int(part) for part in fh.read().split()]
                except (OSError, ValueError):
                    time.sleep(0.05)
            self.assertEqual(len(pids), 2, "target chain never recorded both pids")
            for pid in pids:
                self.addCleanup(force_kill, pid)
                self.assertTrue(wait_gone(pid),
                                f"descendant {pid} survived the target's group kill")

    def test_group_kill_stays_inside_the_spawned_group(self):
        own_pgid = os.getpgid(0)
        # Sibling shares this process' group; a killpg scoped to the spawned
        # group must leave it (and this process) untouched.
        sibling = subprocess.Popen(SLEEPER, stdout=subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL)
        try:
            sample = startup.measure_once(SLEEPER, timeout=0.4)
            self.assertEqual(sample["outcome"], "timeout", sample)
            time.sleep(0.2)
            os.kill(sibling.pid, 0)
        except ProcessLookupError:
            self.fail("the harness killed a process outside the spawned group")
        finally:
            if sibling.poll() is None:
                sibling.kill()
            sibling.wait(timeout=30)
        self.assertEqual(os.getpgid(0), own_pgid)


@unittest.skipUnless(PS, "zombie-state inspection needs ps(1)")
class ZombieInspectionIsUsable(unittest.TestCase):
    def test_ps_reports_this_process_and_no_zombies_after_a_normal_run(self):
        startup.run_batch(BENIGN, 1, timeout=60)
        self.assertEqual(zombie_child_pids(), [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
