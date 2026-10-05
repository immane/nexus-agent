#!/usr/bin/env python3
"""Gap tests for timeout/resource paths in tools/perf/readiness.py.

Run from the repository root:
    python3 -m unittest discover -s tools/perf -p 'test_*.py'
or directly:
    python3 tools/perf/test_readiness_timeouts_gaps.py

Scope, complementary to test_readiness.py:
- idle CPU/RSS high-water is sampled only while the target stays alive
- cancel "exited within window" versus window expiry while still alive
- bounded waits (terminate escalation, wait4 reaping) cannot hang the run

Every PTY target here is a benign pseudo executable built from the test
process's own Python interpreter plus a generated script; no repository
binary, network, docker, sudo, or cache action is used. Wait bounds are
generous relative to the injected timeout values so the tests stay
deterministic on a loaded machine.
"""

import os
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import readiness  # noqa: E402

POSIX = os.name == "posix" and hasattr(os, "fork") and hasattr(os, "wait4")

# A tiny ratatui-shaped frame whose visible markers match
# readiness.DEFAULT_REQUIRED, with a footer that contains generic
# "ctrl+c cancel" text so a redraw can never satisfy cancel evidence.
FRAME = (
    "\x1b[?1049h"
    "\x1b[1;1H nexus-tui  M0-TEST run:run-1"
    "\x1b[3;1H composer (fixed) "
    "\x1b[5;1H m0-test  ctrl+c cancel  tab focus  ctrl+d quit "
)

# Stays alive across the whole idle window, then exits on quit keys.
BODY_IDLE_ALIVE_THEN_QUIT = r'''
import sys, time
sys.stdout.write(FRAME); sys.stdout.flush()
time.sleep(4)
'''

# Emits the frame, then exits early inside the idle window.
BODY_IDLE_EXIT_EARLY = r'''
import sys, time
sys.stdout.write(FRAME); sys.stdout.flush()
time.sleep(0.15)
'''

# Frame, then immediate exit with no further output.
BODY_FRAME_THEN_EXIT = r'''
import sys
sys.stdout.write(FRAME); sys.stdout.flush()
'''

# No frame at all: readiness never completes, so idle sampling must not run.
BODY_NO_FRAME_SLEEP = r'''
import sys, time
sys.stdout.write("starting up without a frame\r\n"); sys.stdout.flush()
time.sleep(30)
'''

# Frame plus ballast so live RSS sampling has something to observe.
BODY_IDLE_BALLAST_ALIVE = r'''
import sys, time
ballast = bytearray(8 << 20)
ballast[0] = 1
sys.stdout.write(FRAME); sys.stdout.flush()
time.sleep(4)
'''

# Frame, then stays alive across the whole cancel window while ignoring the
# PTY-generated SIGINT from Ctrl+C. Canonical mode keeps ISIG enabled, so a
# target that does not ignore SIGINT is killed by the cancel key itself and
# cannot exercise the window-expiry-while-alive path.
BODY_CANCEL_IGNORED_ALIVE = r'''
import signal, sys, time
signal.signal(signal.SIGINT, signal.SIG_IGN)
signal.signal(signal.SIGQUIT, signal.SIG_IGN)
sys.stdout.write(FRAME); sys.stdout.flush()
time.sleep(4)
'''

# Cancel evidence marker, then the target keeps running (no exit).
BODY_CANCEL_ACK_STAYS_ALIVE = r'''
import sys, tty
sys.stdout.write(FRAME); sys.stdout.flush()
tty.setraw(0)
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b == b"\x04":
        break
    if b == b"\x03":
        sys.stdout.write("Status: Cancelled\r\n"); sys.stdout.flush()
'''

# Cancel evidence marker, then the target exits on its own.
BODY_CANCEL_ACK_THEN_EXIT = r'''
import sys, time, tty
sys.stdout.write(FRAME); sys.stdout.flush()
tty.setraw(0)
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b == b"\x04":
        break
    if b == b"\x03":
        sys.stdout.write("Status: Cancelled\r\n"); sys.stdout.flush()
        time.sleep(0.1)
        break
'''

# Exits on cancel without ever printing evidence.
BODY_CANCEL_SILENT_EXIT = r'''
import sys, tty
sys.stdout.write(FRAME); sys.stdout.flush()
tty.setraw(0)
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b == b"\x04":
        break
    if b == b"\x03":
        sys.exit(3)
'''

# Never produces a frame, never exits, and ignores both quit keys and SIGTERM.
BODY_STUBBORN_NO_FRAME = r'''
import signal, sys, time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
sys.stdout.write("no frame here\r\n"); sys.stdout.flush()
while True:
    time.sleep(0.1)
'''


def make_script(directory, name, body):
    path = os.path.join(directory, name)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write("FRAME = " + repr(FRAME) + "\n" + body)
    return path


class _FakeTarget:
    """Minimal PtyTarget stand-in for bounded-wait tests (no real child).

    check_exit() returns False until `exit_after` polls have happened, then
    reports the exit once and stays reaped. The pid is deliberately
    nonexistent so any real signal the harness sends is a no-op OSError.
    """

    def __init__(self, exit_after=None):
        self.pid = 2 ** 22
        self.master = -1
        self.reaped = False
        self.exit_code = None
        self.signal = None
        self.exited_ns = None
        self.polls = 0
        self.writes = []
        self._exit_after = exit_after

    def write(self, data):
        self.writes.append(data)
        return True

    def check_exit(self):
        if self.reaped:
            return True
        self.polls += 1
        if self._exit_after is not None and self.polls >= self._exit_after:
            self.reaped = True
            self.exit_code = 0
            self.exited_ns = time.monotonic_ns()
        return self.reaped


@unittest.skipUnless(POSIX, "PTY harness requires POSIX fork/openpty/wait4")
class PtyTestCase(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls._tmp = tempfile.TemporaryDirectory(prefix="nexus-readiness-gaps-")
        cls.tmp = cls._tmp.name

    @classmethod
    def tearDownClass(cls):
        cls._tmp.cleanup()

    def script(self, name, body):
        return make_script(self.tmp, name, body)

    def run_target(self, script, **overrides):
        config = readiness.RunConfig(**overrides)
        return readiness.run_measurement(config, [sys.executable, "-u", script], 0)

    def assert_reaped(self, result):
        self.assertIsNotNone(result.get("pid"))
        with self.assertRaises(ProcessLookupError):
            os.kill(result["pid"], 0)


class IdleSamplingWhileAliveTests(PtyTestCase):
    """Idle CPU/RSS is only meaningful while the target stays alive."""

    def test_idle_is_observed_when_target_alive_through_whole_window(self):
        script = self.script("idle_alive.py", BODY_IDLE_ALIVE_THEN_QUIT)
        result = self.run_target(
            script,
            mode="idle",
            idle_window=0.5,
            idle_interval=0.1,
            timeout=5.0,
            quit_timeout=0.2,
            kill_grace=0.5,
        )
        idle = result["idle"]
        self.assertFalse(idle["exited_during_window"])
        self.assertEqual(idle["status"], "observed")
        self.assertGreaterEqual(idle["window_s"], 0.5 * 0.9)
        self.assertGreater(idle["rss_samples"], 0)
        self.assertTrue(
            idle["cpu_seconds"] is None or idle["cpu_seconds"] >= 0.0,
            "idle CPU delta must never be negative",
        )
        self.assert_reaped(result)

    def test_idle_window_expiry_while_alive_is_not_marked_short(self):
        script = self.script("idle_alive2.py", BODY_IDLE_ALIVE_THEN_QUIT)
        result = self.run_target(
            script,
            mode="idle",
            idle_window=0.4,
            idle_interval=0.1,
            timeout=5.0,
            quit_timeout=0.2,
            kill_grace=0.5,
        )
        idle = result["idle"]
        # The target outlived the window, so the window itself is complete
        # and stays distinguishable from an exit inside the window.
        self.assertFalse(idle["exited_during_window"])
        self.assertEqual(idle["status"], "observed")
        self.assertLess(idle["window_s"], 0.4 * 1.5)
        self.assertEqual(result["exit"]["status"], "exited")
        self.assert_reaped(result)

    def test_exit_inside_window_shortens_window_and_stops_sampling(self):
        script = self.script("idle_early_exit.py", BODY_IDLE_EXIT_EARLY)
        result = self.run_target(
            script,
            mode="idle",
            idle_window=2.0,
            idle_interval=0.05,
            timeout=5.0,
        )
        idle = result["idle"]
        self.assertTrue(idle["exited_during_window"])
        self.assertEqual(idle["status"], "short")
        self.assertLess(idle["window_s"], 2.0 * 0.9)
        # Live sampling stops once the child is gone: at most the samples
        # taken before the exit poll can be recorded.
        self.assertLessEqual(idle["rss_samples"], 2)
        self.assertTrue(
            idle["cpu_seconds"] is None or idle["cpu_seconds"] >= 0.0,
            "CPU delta sampled across an exit must never go negative",
        )
        self.assertEqual(result["exit"]["status"], "exited")
        self.assert_reaped(result)

    def test_high_water_rss_survives_early_exit_while_live_peak_may_not(self):
        script = self.script("idle_early_exit2.py", BODY_IDLE_EXIT_EARLY)
        result = self.run_target(
            script,
            mode="idle",
            idle_window=2.0,
            idle_interval=0.05,
            timeout=5.0,
        )
        # ru_maxrss comes from the wait4 reaping of the exited child, so the
        # high-water fact is still reported even when live sampling stopped.
        self.assertIsNotNone(result["resources"]["ru_maxrss"])
        self.assertGreater(result["resources"]["ru_maxrss"], 0)
        self.assertTrue(result["idle"]["exited_during_window"])
        self.assert_reaped(result)

    def test_no_idle_sampling_without_observed_readiness(self):
        script = self.script("no_frame2.py", BODY_NO_FRAME_SLEEP)
        result = self.run_target(
            script,
            mode="idle",
            idle_window=0.3,
            idle_interval=0.1,
            ready_timeout=0.3,
            timeout=5.0,
            quit_timeout=0.2,
            kill_grace=0.5,
        )
        self.assertEqual(result["ready"]["status"], "ready-timeout")
        self.assertIsNone(result["idle"])
        self.assert_reaped(result)

    @unittest.skipUnless(
        sys.platform.startswith("linux"), "VmHWM peak RSS is Linux-only"
    )
    def test_live_peak_rss_does_not_exceed_wait4_high_water(self):
        script = self.script("idle_ballast.py", BODY_IDLE_BALLAST_ALIVE)
        result = self.run_target(
            script,
            mode="idle",
            idle_window=0.5,
            idle_interval=0.1,
            timeout=5.0,
            quit_timeout=0.2,
            kill_grace=0.5,
        )
        peak = result["idle"]["rss_peak_sampled_bytes"]
        self.assertIsNotNone(peak)
        self.assertGreater(peak, 0)
        # ru_maxrss is kilobytes on Linux; the sampled VmHWM is bytes for the
        # same child, so a same-process high-water cannot exceed it.
        self.assertLessEqual(peak, result["resources"]["ru_maxrss"] * 1024)


class CancelWindowTests(PtyTestCase):
    """exited_within_window must distinguish exit from window expiry."""

    def test_skipped_when_already_exited_reports_exited_within_window(self):
        script = self.script("cancel_exits.py", BODY_FRAME_THEN_EXIT)
        result = self.run_target(script, mode="cancel", cancel_settle=0.2)
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "skipped")
        self.assertTrue(cancel["exited_within_window"])
        self.assertIsNone(cancel["wrote_keys"])
        self.assertIsNone(cancel["cancel_marker_observed_ms"])
        self.assertIsNone(cancel["cancel_to_exit_ms"])

    def test_observed_marker_with_live_target_has_no_exit_latency(self):
        script = self.script("cancel_ack_alive.py", BODY_CANCEL_ACK_STAYS_ALIVE)
        result = self.run_target(
            script,
            mode="cancel",
            cancel_timeout=0.5,
            quit_timeout=0.2,
            kill_grace=0.5,
            timeout=5.0,
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "observed")
        self.assertIsNotNone(cancel["cancel_marker_observed_ms"])
        self.assertFalse(cancel["exited_within_window"])
        self.assertIsNone(cancel["cancel_to_exit_ms"])
        # Evidence is any-of, so missing_evidence lists the alternatives that
        # were not seen; the matched literal must not appear in it.
        self.assertNotIn("status: cancelled", cancel["missing_evidence"])
        self.assertIn("run cancelled", cancel["missing_evidence"])
        self.assert_reaped(result)

    def test_observed_marker_then_exit_reports_both_marker_and_exit(self):
        script = self.script("cancel_ack_exit.py", BODY_CANCEL_ACK_THEN_EXIT)
        result = self.run_target(
            script,
            mode="cancel",
            cancel_timeout=2.0,
            timeout=5.0,
            quit_timeout=0.2,
            kill_grace=0.5,
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "observed")
        self.assertIsNotNone(cancel["cancel_marker_observed_ms"])
        self.assertTrue(cancel["exited_within_window"])
        self.assertIsNotNone(cancel["cancel_to_exit_ms"])
        self.assertGreaterEqual(cancel["cancel_to_exit_ms"], 0.0)
        self.assertIn("cannot distinguish", cancel["confound"])
        self.assert_reaped(result)

    def test_unconfirmed_on_exit_without_evidence_marks_exited_within_window(self):
        script = self.script("cancel_silent_exit.py", BODY_CANCEL_SILENT_EXIT)
        result = self.run_target(script, mode="cancel", cancel_timeout=2.0)
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "unconfirmed")
        self.assertTrue(cancel["exited_within_window"])
        self.assertIn("without observed cancel evidence", cancel["reason"])
        # Unconfirmed never fabricates a latency, exit or marker.
        self.assertIsNone(cancel["cancel_marker_observed_ms"])
        self.assertIsNone(cancel["cancel_to_exit_ms"])
        self.assertEqual(result["exit"]["exit_code"], 3)
        self.assert_reaped(result)

    def test_unconfirmed_on_window_expiry_while_alive_is_not_an_exit(self):
        script = self.script("cancel_alive.py", BODY_CANCEL_IGNORED_ALIVE)
        result = self.run_target(
            script,
            mode="cancel",
            cancel_timeout=0.3,
            timeout=5.0,
            quit_timeout=0.2,
            kill_grace=0.5,
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "unconfirmed")
        self.assertFalse(cancel["exited_within_window"])
        self.assertIn("still alive", cancel["reason"])
        self.assertIsNone(cancel["cancel_marker_observed_ms"])
        self.assertIsNone(cancel["cancel_to_exit_ms"])
        self.assertTrue(result["exit"]["cleanup"]["escalated"])
        self.assert_reaped(result)


class BoundedWaitTests(PtyTestCase):
    """Every escalation stage is time-bounded; nothing waits forever."""

    def test_terminate_escalates_to_sigkill_within_bounds(self):
        target = _FakeTarget()
        cfg = readiness.RunConfig(quit_timeout=0.2, kill_grace=0.2)
        drains = []

        def drain(_timeout_s):
            drains.append(_timeout_s)

        started = time.monotonic()
        cleanup = readiness.terminate_target(target, cfg, drain)
        elapsed = time.monotonic() - started
        self.assertEqual(cleanup["method"], "sigkill")
        self.assertTrue(cleanup["escalated"])
        self.assertFalse(cleanup["reaped"])
        self.assertEqual(target.writes, [cfg.quit_keys])
        self.assertLess(elapsed, 0.2 + 2 * 0.2 + 1.0)
        self.assertGreater(len(drains), 0)

    def test_terminate_reports_reaped_when_quit_keys_settle_it(self):
        target = _FakeTarget(exit_after=2)
        cfg = readiness.RunConfig(quit_timeout=0.5, kill_grace=0.5)
        started = time.monotonic()
        cleanup = readiness.terminate_target(target, cfg, lambda _t: None)
        elapsed = time.monotonic() - started
        self.assertEqual(cleanup["method"], "quit-keys")
        self.assertFalse(cleanup["escalated"])
        self.assertTrue(cleanup["reaped"])
        self.assertLess(elapsed, 0.5 + 0.5)

    def test_terminate_reports_sigterm_stage_before_sigkill(self):
        target = _FakeTarget(exit_after=30)

        def drain(_timeout_s):
            time.sleep(0.01)

        cfg = readiness.RunConfig(quit_timeout=0.15, kill_grace=0.6)
        started = time.monotonic()
        cleanup = readiness.terminate_target(target, cfg, drain)
        elapsed = time.monotonic() - started
        self.assertEqual(cleanup["method"], "sigterm")
        self.assertTrue(cleanup["escalated"])
        self.assertTrue(cleanup["reaped"])
        self.assertLess(elapsed, 0.15 + 0.6 + 1.0)

    def test_terminate_is_a_noop_for_an_already_reaped_target(self):
        target = _FakeTarget()
        target.reaped = True
        cfg = readiness.RunConfig(quit_timeout=5.0, kill_grace=5.0)
        started = time.monotonic()
        cleanup = readiness.terminate_target(target, cfg, lambda _t: None)
        elapsed = time.monotonic() - started
        self.assertEqual(cleanup["method"], "already-exited")
        self.assertFalse(cleanup["escalated"])
        self.assertTrue(cleanup["reaped"])
        self.assertEqual(target.writes, [])
        self.assertLess(elapsed, 1.0)

    def test_overall_timeout_bounds_the_idle_window(self):
        script = self.script("overall_timeout.py", BODY_IDLE_ALIVE_THEN_QUIT)
        started = time.monotonic()
        result = self.run_target(
            script,
            mode="idle",
            idle_window=30.0,
            idle_interval=0.1,
            timeout=0.5,
            quit_timeout=0.2,
            kill_grace=0.5,
        )
        elapsed = time.monotonic() - started
        self.assertEqual(result["outcome"], "run-timeout")
        self.assertLess(result["idle"]["window_s"], 30.0)
        self.assertLess(elapsed, 5.0)
        self.assertEqual(result["exit"]["status"], "exited")
        self.assert_reaped(result)

    def test_stubborn_target_still_returns_within_overall_bound(self):
        script = self.script("stubborn2.py", BODY_STUBBORN_NO_FRAME)
        started = time.monotonic()
        result = self.run_target(
            script,
            ready_timeout=0.3,
            timeout=2.0,
            quit_timeout=0.2,
            kill_grace=0.5,
        )
        elapsed = time.monotonic() - started
        self.assertEqual(result["ready"]["status"], "ready-timeout")
        self.assertEqual(result["exit"]["cleanup"]["method"], "sigkill")
        self.assertTrue(result["exit"]["cleanup"]["escalated"])
        self.assertLess(elapsed, 5.0)
        self.assert_reaped(result)

    def test_check_exit_on_an_already_reaped_child_does_not_block(self):
        pid = os.fork()
        if pid == 0:  # pragma: no cover - child never returns
            os._exit(0)
        os.wait4(pid, 0)
        target = readiness.PtyTarget(pid=pid, master=-1, started_ns=time.monotonic_ns())
        started = time.monotonic()
        self.assertTrue(target.check_exit())
        elapsed = time.monotonic() - started
        self.assertTrue(target.reaped)
        self.assertIsNone(target.exit_code)
        self.assertIsNone(target.ru_maxrss)
        self.assertIsNone(target.read_error)
        self.assertLess(elapsed, 1.0)
        # Idempotent: a reaped target keeps reporting the terminal state.
        self.assertTrue(target.check_exit())


if __name__ == "__main__":
    unittest.main()