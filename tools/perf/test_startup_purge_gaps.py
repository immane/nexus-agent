#!/usr/bin/env python3
"""Gap tests for purge handling in tools/perf/startup.py (stdlib unittest only).

Companion to tools/perf/test_startup.py, which pins the *first* purge
failure (immediate nonzero purge, a bounded purge timeout, partial samples
via run_batch, and a single cold-mode CLI failure). This file covers the
remaining purge paths that were left unasserted:

- run_purge evidence on its own (success, nonzero, signal death, timeout);
- a purge that fails on a *later* sample, with partial samples kept in the
  JSON record and no summary built from them;
- a purge timeout observed through the CLI (exit status, detail text, wall
  clock) and process-group cleanup of the purge's own descendants;
- exit-status precedence (cold invalidation 2 over invalid-sample 1) and
  warm-mode independence in --mode both;
- labeling: a failed purge is never labeled like a successful one, even when
  earlier purges exited 0, and cold runs without a purge stay "unprepared";
- warm mode does not merely record the purge as not-run, it never executes
  it.

Deterministic: only local benign processes (this interpreter, POSIX shell
builtins, a bounded sleep), temp dirs, and short wall-clock bounds. No
network, no repository binary, no privileged or cache-purging action.

Run from the repository root:
    python3 -m unittest discover -s tools/perf -p 'test_*.py'
or directly:
    python3 tools/perf/test_startup_purge_gaps.py
"""

import json
import os
import shlex
import signal
import subprocess
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import startup  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "startup.py")
POSIX_SHELL = os.name == "posix"
POSIX_KILLPG = POSIX_SHELL and hasattr(os, "killpg")
SIGKILL = getattr(signal, "SIGKILL", None)
# Shell-quoted no-op purge: succeeds without needing any external binary.
PY_PURGE = f"{shlex.quote(sys.executable)} -c pass"

# Cache_prep labels the harness uses. A failed purge must land on none of the
# labels that describe a completed or still-pending prep.
CACHE_PREP_PENDING = "purge configured; result pending (cache reset not verified)"
CACHE_PREP_ALL_OK = ("user-supplied purge command exited 0 for every sample "
                     "(cache reset not verified)")
CACHE_PREP_NONE = "none"
CACHE_PREP_FAILED = "purge failed; cache reset not verified"

# Purge that leaves a grandchild behind, so group cleanup is observable.
_PURGE_WITH_GRANDCHILD = (
    "import subprocess, sys, time\n"
    "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])\n"
    "with open(sys.argv[1], 'w') as fh:\n"
    "    fh.write(str(child.pid))\n"
    "time.sleep(60)\n"
)


def _counter_purge(counter_path, fail_at):
    """POSIX-shell purge that exits 0 for its first `fail_at - 1` runs.

    An alternating marker command can only fail on the second run; a counter
    file reaches any later sample deterministically, which is what a
    "purge failed after some samples were already measured" case needs.
    """
    quoted = shlex.quote(counter_path)
    return (f"n=0; [ -r {quoted} ] && n=$(cat {quoted}); "
            f"n=$((n + 1)); echo \"$n\" > {quoted}; [ \"$n\" -lt {fail_at} ]")


def _wait_for_pid(path, deadline):
    """Poll `path` until it holds a pid; None when the deadline passes."""
    while time.monotonic() < deadline:
        try:
            with open(path, encoding="utf-8") as fh:
                return int(fh.read().strip())
        except (OSError, ValueError):
            time.sleep(0.05)
    return None


def _wait_until_gone(pid, deadline):
    """True once `pid` no longer exists."""
    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return True
        except PermissionError:
            pass
        time.sleep(0.05)
    return False


def _kill_quiet(pid):
    try:
        os.kill(pid, signal.SIGKILL)
    except OSError:
        pass


class CliGapTestCase(unittest.TestCase):
    """Subprocess driver for the CLI-level purge gap tests."""

    def _run_cli(self, *args, timeout=120):
        return subprocess.run(
            [sys.executable, SCRIPT, *args],
            capture_output=True, text=True, timeout=timeout,
        )


@unittest.skipUnless(POSIX_SHELL, "purge command uses a POSIX shell")
class RunPurgeEvidenceTests(unittest.TestCase):
    """run_purge is otherwise only reached through run_batch."""

    def test_success_records_command_and_zero_status(self):
        evidence = startup.run_purge(PY_PURGE, timeout=30)
        self.assertEqual(evidence["command"], PY_PURGE)
        self.assertEqual(evidence["exit_code"], 0)
        self.assertIsNone(evidence["signal"])
        self.assertGreaterEqual(evidence["elapsed_ms"], 0.0)

    def test_nonzero_exit_raises_with_exit_code_and_command_evidence(self):
        with self.assertRaises(startup.PurgeError) as ctx:
            startup.run_purge("exit 5", timeout=30)
        exc = ctx.exception
        self.assertEqual(exc.reason, "nonzero")
        self.assertEqual(exc.exit_code, 5)
        self.assertIsNone(exc.signal)
        self.assertEqual(exc.command, "exit 5")
        self.assertGreaterEqual(exc.elapsed_ms, 0.0)
        self.assertIn("exit_code=5", exc.detail)
        # run_purge alone knows no batch position; run_batch fills these in.
        self.assertIsNone(exc.sample_index)
        self.assertEqual(exc.samples, [])

    def test_signal_death_is_a_failure_with_signal_evidence(self):
        # A purge killed by a signal did not prepare the cache either, and the
        # record must keep the signal instead of inventing an exit code.
        with self.assertRaises(startup.PurgeError) as ctx:
            startup.run_purge("kill -TERM $$", timeout=30)
        exc = ctx.exception
        self.assertEqual(exc.reason, "nonzero")
        self.assertIsNone(exc.exit_code)
        self.assertEqual(exc.signal, int(signal.SIGTERM))
        self.assertIn(f"signal={int(signal.SIGTERM)}", exc.detail)

    def test_timeout_is_bounded_and_reports_no_exit_code(self):
        started = time.monotonic()
        with self.assertRaises(startup.PurgeError) as ctx:
            startup.run_purge("sleep 30", timeout=0.3)
        exc = ctx.exception
        self.assertEqual(exc.reason, "timeout")
        self.assertIn("did not exit within 0.3s", exc.detail)
        self.assertIn("termination requested", exc.detail)
        self.assertEqual(exc.command, "sleep 30")
        self.assertIsNone(exc.exit_code)
        # Terminated, not exited: a kill signal, or no status at all if the
        # watchdog race won. Either way never exit_code 0.
        self.assertIn(exc.signal, (None, SIGKILL))
        self.assertLess(time.monotonic() - started, 10.0)


@unittest.skipUnless(POSIX_SHELL, "purge command uses a POSIX shell")
class ColdPurgeNonzeroExitTests(CliGapTestCase):
    """A nonzero purge must invalidate cold mode with exit status 2."""

    def test_nonzero_purge_reports_no_cold_summary_or_success_note(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self._run_cli(
                sys.executable, "-n", "3", "--mode", "cold",
                "--purge-cmd", "exit 4", "--timeout", "30",
                "--json", out, "--", "-c", "pass",
            )
            self.assertEqual(proc.returncode, 2, proc.stderr)
            with open(out, encoding="utf-8") as fh:
                record = json.load(fh)
        entry = record["modes"][0]
        self.assertEqual(entry["mode"], "cold")
        self.assertFalse(entry["valid"])
        self.assertTrue(entry["invalid_reason"].startswith("purge nonzero"))
        self.assertEqual(entry["purge_error"]["reason"], "nonzero")
        self.assertEqual(entry["purge_error"]["exit_code"], 4)
        self.assertEqual(entry["purge_error"]["failed_at_sample"], 0)
        self.assertEqual(entry["purge_runs"], 1)
        self.assertEqual(entry["samples"], [])
        # No cold timing may be published for a mode that never measured.
        self.assertIsNone(entry["results"])
        self.assertIn("invalid: purge nonzero", proc.stdout)
        self.assertIn("at sample 0, 0 collected", proc.stdout)
        self.assertNotIn("[cold] successful=", proc.stdout)
        self.assertNotIn("p50=", proc.stdout)
        self.assertNotIn("exited 0 for every sample", proc.stdout)

    def test_purge_failure_invalidates_only_the_cold_mode(self):
        # --mode both measures warm first; a cold purge failure must not
        # discard warm results or mark warm invalid.
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self._run_cli(
                sys.executable, "-n", "1", "--mode", "both",
                "--purge-cmd", "exit 3", "--timeout", "30",
                "--json", out, "--", "-c", "pass",
            )
            self.assertEqual(proc.returncode, 2, proc.stderr)
            with open(out, encoding="utf-8") as fh:
                record = json.load(fh)
        warm, cold = record["modes"]
        self.assertEqual([warm["mode"], cold["mode"]], ["warm", "cold"])
        self.assertTrue(warm["valid"])
        self.assertIsNone(warm["invalid_reason"])
        self.assertEqual(warm["cache_prep"], CACHE_PREP_NONE)
        self.assertEqual(warm["results"]["successful"], 1)
        self.assertEqual(warm["results"]["invalid"], {})
        self.assertFalse(cold["valid"])
        self.assertIsNone(cold["results"])
        self.assertIn("[warm] successful=1/1", proc.stdout)

    def test_purge_failure_exit_2_takes_precedence_over_invalid_sample_exit_1(self):
        # Every warm sample is a nonzero exit (which alone would exit 1); the
        # cold purge failure must still surface as 2, the "cold not measured"
        # status, while warm keeps its failed-sample evidence.
        proc = self._run_cli(
            sys.executable, "-n", "3", "--mode", "both",
            "--purge-cmd", "exit 3", "--timeout", "30",
            "--", "-c", "raise SystemExit(3)",
        )
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertIn("[warm] successful=0/3", proc.stdout)
        self.assertIn("[cold] invalid: purge nonzero", proc.stdout)
        self.assertIn("[warm] invalid sample #0: outcome=nonzero", proc.stdout)


@unittest.skipUnless(POSIX_SHELL, "purge command uses a POSIX shell")
class ColdPurgeTimeoutTests(CliGapTestCase):
    """A purge that overruns --timeout is bounded and invalidates cold."""

    def test_purge_timeout_exits_2_within_a_bounded_wall_clock(self):
        started = time.monotonic()
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self._run_cli(
                sys.executable, "-n", "2", "--mode", "cold",
                "--purge-cmd", "sleep 30", "--timeout", "0.3",
                "--json", out, "--", "-c", "pass",
            )
            self.assertEqual(proc.returncode, 2, proc.stderr)
            with open(out, encoding="utf-8") as fh:
                record = json.load(fh)
        self.assertLess(time.monotonic() - started, 30.0)
        entry = record["modes"][0]
        self.assertFalse(entry["valid"])
        self.assertTrue(entry["invalid_reason"].startswith("purge timeout"))
        error = entry["purge_error"]
        self.assertEqual(error["reason"], "timeout")
        self.assertIn("did not exit within 0.3s", error["detail"])
        self.assertIn("termination requested", error["detail"])
        # A purge that never exited cannot report an exit code.
        self.assertIsNone(error["exit_code"])
        self.assertIn(error["signal"], (None, SIGKILL))
        self.assertEqual(error["failed_at_sample"], 0)
        self.assertEqual(entry["purge_runs"], 1)
        self.assertEqual(entry["samples"], [])
        self.assertIsNone(entry["results"])
        self.assertIn("[cold] invalid: purge timeout", proc.stdout)
        self.assertNotIn("exited 0 for every sample", proc.stdout)
        self.assertNotIn("[cold] successful=", proc.stdout)

    @unittest.skipUnless(POSIX_KILLPG, "purge group kill requires POSIX killpg")
    def test_timed_out_purge_kills_its_own_process_group(self):
        with tempfile.TemporaryDirectory() as tmp:
            pidfile = os.path.join(tmp, "purge-child.pid")
            helper = os.path.join(tmp, "purge_parent.py")
            with open(helper, "w", encoding="utf-8") as fh:
                fh.write(_PURGE_WITH_GRANDCHILD)
            cmd = (f"{shlex.quote(sys.executable)} {shlex.quote(helper)} "
                   f"{shlex.quote(pidfile)}")
            with self.assertRaises(startup.PurgeError) as ctx:
                startup.run_purge(cmd, timeout=1.0)
            self.assertEqual(ctx.exception.reason, "timeout")
            pid = _wait_for_pid(pidfile, deadline=time.monotonic() + 5.0)
            self.assertIsNotNone(pid, "purge never recorded its grandchild pid")
            try:
                self.assertTrue(
                    _wait_until_gone(pid, time.monotonic() + 5.0),
                    "grandchild of the timed-out purge survived the group kill",
                )
            finally:
                _kill_quiet(pid)


@unittest.skipUnless(POSIX_SHELL, "purge command uses a POSIX shell")
class PartialSampleEvidenceTests(CliGapTestCase):
    """A purge failing on a later sample keeps the earlier ones as evidence."""

    def test_partial_samples_are_kept_without_a_cold_summary(self):
        with tempfile.TemporaryDirectory() as tmp:
            counter = os.path.join(tmp, "purge-count")
            out = os.path.join(tmp, "perf.json")
            proc = self._run_cli(
                sys.executable, "-n", "4", "--mode", "cold",
                "--purge-cmd", _counter_purge(counter, fail_at=3),
                "--timeout", "30", "--json", out, "--", "-c", "pass",
            )
            self.assertEqual(proc.returncode, 2, proc.stderr)
            with open(out, encoding="utf-8") as fh:
                text = fh.read()
            self.assertNotIn("NaN", text)
            record = json.loads(text)
        entry = record["modes"][0]
        # Purges 1 and 2 succeeded, the third failed: two samples survive.
        self.assertEqual([s["index"] for s in entry["samples"]], [0, 1])
        self.assertEqual([s["outcome"] for s in entry["samples"]], ["ok", "ok"])
        # purge_runs counts attempted purges, the failed one included.
        self.assertEqual(entry["purge_runs"], 3)
        self.assertEqual(entry["purge_error"]["failed_at_sample"], 2)
        self.assertIsNotNone(entry["purge_error"]["elapsed_ms"])
        # A partial batch is evidence, never a measured cold mode.
        self.assertIsNone(entry["results"])
        self.assertIn("at sample 2, 2 collected", proc.stdout)

    def test_invalid_target_sample_survives_a_later_purge_failure(self):
        with tempfile.TemporaryDirectory() as tmp:
            ran = shlex.quote(os.path.join(tmp, "target-ran"))
            script = (f"if [ -e {ran} ]; then exit 0; "
                      f"else : > {ran}; exit 7; fi")
            counter = os.path.join(tmp, "purge-count")
            out = os.path.join(tmp, "perf.json")
            # Target argv is "/bin/sh -c <script>": a `-n` here would make sh
            # parse only, so the target would always exit 0 and the
            # nonzero sample this test needs would never be produced.
            proc = self._run_cli(
                "/bin/sh", "-n", "3", "--mode", "cold",
                "--purge-cmd", _counter_purge(counter, fail_at=2),
                "--timeout", "30", "--json", out, "--", "-c", script,
            )
            self.assertEqual(proc.returncode, 2, proc.stderr)
            with open(out, encoding="utf-8") as fh:
                entry = json.load(fh)["modes"][0]
        self.assertEqual(entry["purge_error"]["failed_at_sample"], 1)
        self.assertEqual(entry["purge_runs"], 2)
        # The failed launch keeps its own outcome and exit code as evidence.
        self.assertEqual(len(entry["samples"]), 1)
        sample = entry["samples"][0]
        self.assertEqual(sample["index"], 0)
        self.assertEqual(sample["outcome"], "nonzero")
        self.assertEqual(sample["exit_code"], 7)
        self.assertIsNone(entry["results"])
        self.assertNotIn("[cold] successful=", proc.stdout)


@unittest.skipUnless(POSIX_SHELL, "purge command uses a POSIX shell")
class PurgeLabelingTests(CliGapTestCase):
    """A failed purge is never labeled like a successful one."""

    def test_purge_failure_after_partial_success_is_never_labeled_success(self):
        with tempfile.TemporaryDirectory() as tmp:
            counter = os.path.join(tmp, "purge-count")
            out = os.path.join(tmp, "perf.json")
            purge_cmd = _counter_purge(counter, fail_at=3)
            proc = self._run_cli(
                sys.executable, "-n", "3", "--mode", "cold",
                "--purge-cmd", purge_cmd, "--timeout", "30",
                "--json", out, "--", "-c", "pass",
            )
            self.assertEqual(proc.returncode, 2, proc.stderr)
            with open(out, encoding="utf-8") as fh:
                record = json.load(fh)
        entry = record["modes"][0]
        # Two purges exited 0, one failed: no label may describe a prep that
        # ran for every sample, and none may imply a reset happened.
        self.assertEqual(entry["cache_prep"], CACHE_PREP_FAILED)
        self.assertNotIn(entry["cache_prep"],
                         {CACHE_PREP_PENDING, CACHE_PREP_ALL_OK, CACHE_PREP_NONE})
        self.assertNotIn("exited 0", entry["purge_error"]["detail"])
        self.assertIn("exit_code=1", entry["purge_error"]["detail"])
        self.assertNotIn("exited 0 for every sample", proc.stdout)
        # The configured command stays in the record as evidence, and the
        # methodology never claims a verified reset.
        self.assertEqual(entry["purge_cmd"], purge_cmd)
        self.assertEqual(record["methodology"]["cold_purge_cmd"], purge_cmd)
        self.assertFalse(record["methodology"]["cache_reset_verified"])
        self.assertIn("cache reset not verified",
                      record["methodology"]["cold_cache_prep"])

    def test_cold_without_purge_cmd_is_labeled_unprepared(self):
        # No --purge-cmd: cold runs must say the cache was not purged instead
        # of implying a reset happened.
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self._run_cli(
                sys.executable, "-n", "1", "--mode", "cold",
                "--timeout", "30", "--json", out, "--", "-c", "pass",
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            with open(out, encoding="utf-8") as fh:
                record = json.load(fh)
        entry = record["modes"][0]
        self.assertTrue(entry["valid"])
        self.assertEqual(entry["cache_prep"], CACHE_PREP_NONE)
        self.assertEqual(entry["purge_runs"], 0)
        self.assertIsNone(entry["purge_cmd"])
        self.assertNotIn("purge_error", entry)
        self.assertEqual(entry["results"]["successful"], 1)
        self.assertFalse(record["methodology"]["cache_reset_verified"])
        self.assertIn("none (cache not purged",
                      record["methodology"]["cold_cache_prep"])
        self.assertIn("OS cache was NOT reset", proc.stdout)


@unittest.skipUnless(POSIX_SHELL, "purge command uses a POSIX shell")
class WarmModeSkipsPurgeTests(CliGapTestCase):
    """A purge configured for cold mode must not run in warm mode."""

    def test_warm_mode_records_configured_purge_without_executing_it(self):
        with tempfile.TemporaryDirectory() as tmp:
            marker = shlex.quote(os.path.join(tmp, "purge-was-run"))
            purge_cmd = f": > {marker}; exit 9"
            out = os.path.join(tmp, "perf.json")
            proc = self._run_cli(
                sys.executable, "-n", "2", "--mode", "warm",
                "--purge-cmd", purge_cmd, "--timeout", "30",
                "--json", out, "--", "-c", "pass",
            )
            # Not-run has to mean not executed: this purge would stamp a file
            # and exit nonzero, either of which would show up here.
            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertFalse(os.path.exists(os.path.join(tmp, "purge-was-run")),
                             "warm mode executed the configured purge command")
            with open(out, encoding="utf-8") as fh:
                record = json.load(fh)
        self.assertEqual([m["mode"] for m in record["modes"]], ["warm"])
        entry = record["modes"][0]
        self.assertTrue(entry["valid"])
        self.assertEqual(entry["purge_runs"], 0)
        self.assertEqual(entry["cache_prep"], CACHE_PREP_NONE)
        self.assertIsNone(entry["purge_cmd"])
        self.assertNotIn("purge_error", entry)
        self.assertEqual(entry["results"]["successful"], 2)
        self.assertEqual([s["index"] for s in entry["samples"]], [0, 1])
        # The configuration is still reported as cold-mode methodology.
        self.assertEqual(record["methodology"]["cold_purge_cmd"], purge_cmd)
        self.assertIn("configured for cold mode",
                      record["methodology"]["cold_cache_prep"])
        self.assertNotIn("invalid: purge", proc.stdout)
        self.assertNotIn("exited 0 for every sample", proc.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)