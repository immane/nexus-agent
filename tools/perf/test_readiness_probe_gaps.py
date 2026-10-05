#!/usr/bin/env python3
"""Gap tests for the frame/input probe paths in tools/perf/readiness.py.

These complement test_readiness.py, which covers the headline cases only.
This file pins the honesty-critical boundaries of the two observation
paths:

- observed-frame-ready marker accounting: all required markers must be
  present in ANSI-filtered output, split markers still count, supporting
  evidence (header / alt screen) is optional and never fabricated, and the
  reported time is anchored to the first observed frame output.
- sentinel-key input probe: every reachable status (observed-key-in-output,
  key-not-observed, exited-before-key, exited-before-probe), the bounded
  post-probe window (including its clipping by the overall run deadline),
  and the rule that the probe is observational: its interpretation may
  describe what appeared in output but must never claim the target
  consumed, processed, or acted on the key.

Run from the repository root:
    python3 -m unittest discover -s tools/perf -p 'test_*.py'
or directly:
    python3 tools/perf/test_readiness_probe_gaps.py

Every PTY target here is a benign pseudo executable built from the test
process's own Python interpreter plus a generated script; no repository
binary, network, docker, sudo, or cache action is used. All waits are
bounded by explicit short timeouts, so no test can hang.
"""

import contextlib
import os
import re
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import readiness  # noqa: E402

POSIX = os.name == "posix" and hasattr(os, "fork") and hasattr(os, "wait4")

# The frame shape matches readiness.DEFAULT_REQUIRED / DEFAULT_HEADER_MARKER
# on purpose: markers below are perturbations of it, so a passing test
# cannot pass by accident on defaults.
FRAME = (
    "\x1b[?1049h"
    "\x1b[1;1H nexus-tui  M0-TEST run:run-1"
    "\x1b[3;1H composer (fixed) "
    "\x1b[5;1H m0-test  ctrl+c cancel  tab focus  ctrl+d quit "
)

# Ready markers present, but the header marker and the alt-screen switch are
# both absent: supporting evidence must stay optional.
FRAME_NO_HEADER_NO_ALT = (
    "\x1b[3;1H composer (fixed) \x1b[5;1H m0-test  ctrl+c cancel "
)

# Header and footer visible, but "composer (fixed)" is swallowed by a CSI
# sequence. The filter consumes ESC [ 1 c as one escape, so only
# "omposer (fixed)" survives and the required marker is genuinely absent
# from the ANSI-filtered stream.
FRAME_MARKER_IN_ESCAPE = (
    "\x1b[?1049h"
    "\x1b[1;1H nexus-tui  M0-TEST run:run-1"
    "\x1b[3;1H \x1b[1composer (fixed)\x1b[0m"
    "\x1b[5;1H m0-test  ctrl+c cancel "
)

BODY_FRAME_WAIT_QUIT = r'''
import sys, tty
sys.stdout.write(FRAME); sys.stdout.flush()
tty.setraw(0)
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b in (b"\x04", b"\x03"):
        break
'''

# Paints a frame with no header marker and no alt-screen sequence.
BODY_FRAME_MARKERS_ONLY = r'''
import sys, tty
sys.stdout.write(FRAME_NO_HEADER_NO_ALT); sys.stdout.flush()
tty.setraw(0)
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b in (b"\x04", b"\x03"):
        break
'''

# Required markers are split across three separate PTY writes, so the
# marker tracker must carry state between reads.
BODY_SPLIT_MARKERS = r'''
import sys, tty, time
sys.stdout.write("\x1b[?1049h"); sys.stdout.flush()
sys.stdout.write("\x1b[1;1H nexus-tui  M0-TEST run:run-1"); sys.stdout.flush()
sys.stdout.write("\x1b[3;1H composer (fix"); sys.stdout.flush()
time.sleep(0.05)
sys.stdout.write("ed)"); sys.stdout.flush()
time.sleep(0.05)
sys.stdout.write(" \x1b[5;1H m0-test  ctrl+c cancel\r\n"); sys.stdout.flush()
tty.setraw(0)
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b in (b"\x04", b"\x03"):
        break
'''

# Frame minus the "m0-test" footer marker, then stays alive.
BODY_FRAME_NO_FOOTER = r'''
import sys, tty, time
sys.stdout.write("\x1b[?1049h"); sys.stdout.flush()
sys.stdout.write("\x1b[1;1H nexus-tui  M0-TEST run:run-1"); sys.stdout.flush()
sys.stdout.write("\x1b[3;1H composer (fixed) \r\n"); sys.stdout.flush()
tty.setraw(0)
while True:
    time.sleep(0.05)
'''

# One required marker is consumed as escape-sequence bytes.
BODY_MARKER_IN_ESCAPE = r'''
import sys, tty, time
sys.stdout.write(FRAME_MARKER_IN_ESCAPE); sys.stdout.flush()
tty.setraw(0)
while True:
    time.sleep(0.05)
'''

BODY_NO_FRAME_EXIT = r'''
import sys
sys.exit(0)
'''

BODY_NO_FRAME_SLEEP = r'''
import sys, time
sys.stdout.write("starting up without a frame\r\n"); sys.stdout.flush()
time.sleep(30)
'''

# Alive after the frame, never reads stdin: the probe key is written and
# consumed by nobody. Raw mode is entered *before* the frame is painted, so
# the tty driver cannot reflect the probe key back as driver echo; otherwise
# whether the echo wins depends on whether the harness writes the key before
# or after this line runs.
BODY_FRAME_IGNORE_INPUT = r'''
import sys, tty, time
tty.setraw(0)
sys.stdout.write(FRAME); sys.stdout.flush()
while True:
    time.sleep(0.05)
'''

# Alive after the frame, exits inside the probe window without echoing the
# sentinel key (raw mode first, for the same reason as above).
BODY_FRAME_EXIT_IN_PROBE_WINDOW = r'''
import sys, tty, time
tty.setraw(0)
sys.stdout.write(FRAME); sys.stdout.flush()
time.sleep(0.25)
'''

# Paints a complete frame, then exits on its own shortly afterwards.
#
# This is the "already gone at probe time" target. On its own it does not
# pin exited-before-probe, because the harness only reaps the child between
# reading the frame and writing the probe key, and that ordering is a
# scheduler race. The contract under test is readiness.run_measurement's
# branch, which reports exited-before-probe when its waited child is already
# reaped; see reaped_after_frame below for how that precondition is supplied
# deterministically.
BODY_FRAME_THEN_EXIT = r'''
import sys, time
sys.stdout.write(FRAME); sys.stdout.flush()
time.sleep(0.05)
'''

# Leaves stdin in canonical mode with ECHO enabled, so the tty driver
# reflects the probe key back even though the target never reads stdin.
BODY_FRAME_ECHO_DRIVER = r'''
import sys, time
sys.stdout.write(FRAME); sys.stdout.flush()
while True:
    time.sleep(0.05)
'''

# Ignores stdin but paints the sentinel char on its own schedule later. Raw
# mode first, so the observed sentinel can only come from the background
# paint and never from driver echo of the probe key.
BODY_FRAME_UNRELATED_SENTINEL = r'''
import sys, tty, time
tty.setraw(0)
sys.stdout.write(FRAME); sys.stdout.flush()
deadline = time.monotonic() + 0.25
while time.monotonic() < deadline:
    time.sleep(0.02)
sys.stdout.write("background paint zzz\r\n"); sys.stdout.flush()
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b in (b"\x04", b"\x03"):
        break
'''

READY_KEYS = frozenset(
    {
        "status",
        "ms",
        "first_visible_ms",
        "required",
        "missing",
        "header_seen",
        "alt_screen_seen",
        "definition",
        "note",
    }
)

PROBE_COMMON_KEYS = frozenset(
    {"status", "ms", "bounded_ms", "char", "wrote_keys", "interpretation"}
)

# A probe interpretation may name consumption only to deny it. Each clause
# that mentions one of these effect terms must also carry a negation.
CONSUMPTION_TERMS = ("consum", "process", "handl", "act on", "acted on", "responsi")
NEGATIONS = ("does not", "do not", "not ", "never", "no ", "cannot")

# Wording that would turn an observation into a claim of proof.
AFFIRMATIVE_CLAIMS = (
    "authoritative",
    "confirms",
    "guarantee",
    "proven",
    "verifies",
)


def make_script(directory, name, body):
    path = os.path.join(directory, name)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write("FRAME = " + repr(FRAME) + "\n")
        handle.write("FRAME_NO_HEADER_NO_ALT = " + repr(FRAME_NO_HEADER_NO_ALT) + "\n")
        handle.write("FRAME_MARKER_IN_ESCAPE = " + repr(FRAME_MARKER_IN_ESCAPE) + "\n")
        handle.write(body)
    return path


@contextlib.contextmanager
def reaped_after_frame(marker=b"m0-test", budget=2.0):
    """Supply "the waited child is already reaped" deterministically.

    readiness.run_measurement reports probe status exited-before-probe when
    the directly waited child is already reaped at the moment the probe would
    write its sentinel key. A real target cannot stage that precondition on
    this platform: the target is the controlling-terminal session leader, so
    when it exits the kernel hangs up the PTY, discards output it wrote
    before exiting, and any forked sibling inherits a revoked slave fd (its
    writes fail with EIO). Reopening the PTY device by name does deliver
    output, but only while the harness is still reading, which is itself a
    scheduler race.

    So the timing precondition is replaced instead of raced for: once the
    required frame marker has actually been read from the PTY (so readiness
    is genuinely observed by this run), check_exit() waits, bounded by
    ``budget``, for the real child to exit and reaps it with the real wait4.
    Everything the test then asserts - the frame observation, the probe
    record, and the reap - is produced by unmodified readiness.py.
    """
    original_read = readiness.PtyTarget.read_chunk
    original_exit = readiness.PtyTarget.check_exit
    state = {"frame_seen": False}

    def read_chunk(self, timeout_s):
        data, eof = original_read(self, timeout_s)
        if marker in data:
            state["frame_seen"] = True
        return data, eof

    def check_exit(self):
        if not state["frame_seen"] or self.reaped:
            return original_exit(self)
        deadline = time.monotonic() + budget
        while time.monotonic() < deadline:
            if original_exit(self):
                return True
            time.sleep(0.002)
        return original_exit(self)

    readiness.PtyTarget.read_chunk = read_chunk
    readiness.PtyTarget.check_exit = check_exit
    try:
        yield
    finally:
        readiness.PtyTarget.read_chunk = original_read
        readiness.PtyTarget.check_exit = original_exit


class ContractAssertions(unittest.TestCase):
    """Shared honesty assertions for observational prose."""

    def assert_no_consumption_claim(self, text):
        lowered = text.lower()
        for clause in re.split(r"[;:,]", lowered):
            if any(term in clause for term in CONSUMPTION_TERMS):
                self.assertTrue(
                    any(negation in clause for negation in NEGATIONS),
                    f"clause claims an input effect without negation: {clause!r}",
                )
        for claim in AFFIRMATIVE_CLAIMS:
            self.assertNotIn(claim, lowered, f"claim of proof in {text!r}")

    def assert_observational_only(self, interpretation):
        self.assertIn("observational", interpretation.lower())
        self.assert_no_consumption_claim(interpretation)


@unittest.skipUnless(POSIX, "PTY harness requires POSIX fork/openpty/wait4")
class PtyTestCase(ContractAssertions, unittest.TestCase):
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


class FrameMarkerGapTests(PtyTestCase):
    def test_ready_schema_is_stable_across_every_status(self):
        observed = self.run_target(self.script("frame.py", BODY_FRAME_WAIT_QUIT))
        timed_out = self.run_target(
            self.script("no_frame.py", BODY_NO_FRAME_SLEEP),
            ready_timeout=0.3,
            timeout=5.0,
            quit_timeout=0.2,
            kill_grace=0.3,
        )
        exited = self.run_target(self.script("silent_exit.py", BODY_NO_FRAME_EXIT))
        statuses = []
        for result in (observed, timed_out, exited):
            statuses.append(result["ready"]["status"])
            with self.subTest(status=result["ready"]["status"]):
                self.assertEqual(set(result["ready"]), set(READY_KEYS))
                self.assert_no_consumption_claim(result["ready"]["note"])
                self.assertIn("required markers", result["ready"]["definition"])
        self.assertEqual(
            statuses, ["observed-frame-ready", "ready-timeout", "exited-before-ready"]
        )

    def test_all_required_markers_are_required_not_just_the_header(self):
        result = self.run_target(
            self.script("no_footer.py", BODY_FRAME_NO_FOOTER),
            ready_timeout=0.3,
            timeout=5.0,
            quit_timeout=0.2,
            kill_grace=0.3,
        )
        ready = result["ready"]
        self.assertEqual(ready["status"], "ready-timeout")
        self.assertIsNone(ready["ms"])
        self.assertEqual(ready["missing"], ["m0-test"])
        self.assertTrue(ready["header_seen"])
        self.assertTrue(ready["alt_screen_seen"])
        self.assertIsNotNone(ready["first_visible_ms"])
        self.assertEqual(result["outcome"], "ready-timeout")
        self.assert_reaped(result)

    def test_marker_bytes_swallowed_by_escape_are_not_frame_evidence(self):
        result = self.run_target(
            self.script("escape_marker.py", BODY_MARKER_IN_ESCAPE),
            ready_timeout=0.3,
            timeout=5.0,
            quit_timeout=0.2,
            kill_grace=0.3,
        )
        ready = result["ready"]
        self.assertEqual(ready["status"], "ready-timeout")
        self.assertIsNone(ready["ms"])
        self.assertEqual(ready["missing"], ["composer (fixed)"])
        # Supporting evidence is present but must not stand in for the
        # missing required marker.
        self.assertTrue(ready["header_seen"])
        self.assertTrue(ready["alt_screen_seen"])
        self.assert_reaped(result)

    def test_markers_split_across_writes_still_observed(self):
        result = self.run_target(self.script("split.py", BODY_SPLIT_MARKERS))
        ready = result["ready"]
        self.assertEqual(ready["status"], "observed-frame-ready")
        self.assertIsNotNone(ready["ms"])
        self.assertEqual(ready["missing"], [])
        self.assertGreaterEqual(ready["ms"], 0.1)
        self.assert_reaped(result)

    def test_supporting_evidence_is_optional_and_never_fabricated(self):
        result = self.run_target(self.script("markers_only.py", BODY_FRAME_MARKERS_ONLY))
        ready = result["ready"]
        self.assertEqual(ready["status"], "observed-frame-ready")
        self.assertEqual(ready["missing"], [])
        self.assertFalse(ready["header_seen"])
        self.assertFalse(ready["alt_screen_seen"])
        self.assert_reaped(result)

    def test_ready_time_is_anchored_to_first_observed_frame_output(self):
        result = self.run_target(self.script("frame.py", BODY_FRAME_WAIT_QUIT))
        ready = result["ready"]
        self.assertEqual(ready["ms"], ready["first_visible_ms"])
        self.assertGreaterEqual(ready["ms"], 0.0)
        self.assertLessEqual(ready["ms"], result["exit"]["elapsed_ms"])
        self.assertEqual(ready["required"], list(readiness.DEFAULT_REQUIRED))
        # The ready record itself must not smuggle in an input-handling claim.
        self.assertIn("not proof", ready["note"])
        self.assertEqual(
            [key for key in ready if "probe" in key or "key" in key], []
        )
        self.assert_reaped(result)


class InputProbeGapTests(PtyTestCase):
    def test_probe_absent_when_not_requested(self):
        result = self.run_target(self.script("frame.py", BODY_FRAME_WAIT_QUIT))
        self.assertEqual(result["ready"]["status"], "observed-frame-ready")
        self.assertIsNone(result["input_probe"])
        self.assert_reaped(result)

    def test_probe_absent_when_frame_never_observed(self):
        timed_out = self.run_target(
            self.script("no_frame.py", BODY_NO_FRAME_SLEEP),
            probe_input=True,
            probe_timeout=0.3,
            ready_timeout=0.3,
            timeout=5.0,
            quit_timeout=0.2,
            kill_grace=0.3,
        )
        exited = self.run_target(
            self.script("silent_exit.py", BODY_NO_FRAME_EXIT),
            probe_input=True,
            probe_timeout=0.3,
        )
        self.assertEqual(timed_out["ready"]["status"], "ready-timeout")
        self.assertIsNone(timed_out["input_probe"])
        self.assertEqual(exited["ready"]["status"], "exited-before-ready")
        self.assertIsNone(exited["input_probe"])
        self.assert_reaped(timed_out)
        self.assert_reaped(exited)

    def test_probe_exits_before_probe_when_target_is_already_gone(self):
        window = 0.5
        script = self.script("frame_then_exit.py", BODY_FRAME_THEN_EXIT)
        with reaped_after_frame():
            result = self.run_target(
                script,
                probe_input=True,
                probe_char="z",
                probe_timeout=window,
            )
        # Readiness is a real observation of this run's PTY output, not a
        # fixture: the marker was read before the target was reaped.
        self.assertEqual(result["ready"]["status"], "observed-frame-ready")
        self.assertEqual(result["ready"]["missing"], [])
        self.assertIsNotNone(result["ready"]["ms"])
        probe = result["input_probe"]
        self.assertEqual(probe["status"], "exited-before-probe")
        # No latency and no key write: the target was already gone.
        self.assertIsNone(probe["ms"])
        self.assertFalse(probe["wrote_keys"])
        self.assertEqual(probe["char"], "z")
        # The configured window is still reported, not a measured duration.
        self.assertEqual(probe["bounded_ms"], window * 1000.0)
        self.assert_observational_only(probe["interpretation"])
        # The definition key is reserved for observed-key-in-output, which is
        # the only status that reports a latency.
        self.assertNotIn("definition", probe)
        self.assertEqual(result["exit"]["cleanup"]["method"], "already-exited")
        self.assert_reaped(result)

    def test_probe_exits_before_key_when_target_dies_in_probe_window(self):
        window = 2.0
        result = self.run_target(
            self.script("dies.py", BODY_FRAME_EXIT_IN_PROBE_WINDOW),
            probe_input=True,
            probe_char="z",
            probe_timeout=window,
        )
        probe = result["input_probe"]
        self.assertEqual(probe["status"], "exited-before-key")
        self.assertIsNone(probe["ms"])
        self.assertTrue(probe["wrote_keys"])
        self.assertEqual(probe["bounded_ms"], window * 1000.0)
        self.assert_observational_only(probe["interpretation"])
        self.assert_reaped(result)

    def test_key_not_observed_window_is_bounded(self):
        window = 0.3
        started = time.monotonic()
        result = self.run_target(
            self.script("ignores.py", BODY_FRAME_IGNORE_INPUT),
            probe_input=True,
            probe_char="z",
            probe_timeout=window,
            quit_timeout=0.2,
            kill_grace=0.4,
        )
        elapsed = time.monotonic() - started
        probe = result["input_probe"]
        self.assertEqual(probe["status"], "key-not-observed")
        self.assertIsNone(probe["ms"])
        self.assertTrue(probe["wrote_keys"])
        self.assertEqual(probe["bounded_ms"], window * 1000.0)
        self.assertIn("bounded post-probe output", probe["interpretation"])
        self.assert_observational_only(probe["interpretation"])
        # A target that never reads the key must not hold the run open
        # beyond the declared window plus bounded cleanup.
        self.assertLess(elapsed, 5.0)
        self.assert_reaped(result)

    def test_probe_window_is_clipped_by_the_overall_run_deadline(self):
        window = 5.0
        started = time.monotonic()
        result = self.run_target(
            self.script("ignores.py", BODY_FRAME_IGNORE_INPUT),
            probe_input=True,
            probe_char="q",
            probe_timeout=window,
            ready_timeout=0.5,
            timeout=0.8,
            quit_timeout=0.2,
            kill_grace=0.4,
        )
        elapsed = time.monotonic() - started
        probe = result["input_probe"]
        # The record declares the configured probe window, while the run
        # itself ends bounded by the shorter overall timeout instead of
        # waiting the full window out.
        self.assertEqual(probe["bounded_ms"], window * 1000.0)
        self.assertEqual(probe["status"], "key-not-observed")
        self.assertIsNone(probe["ms"])
        self.assertEqual(result["outcome"], "run-timeout")
        self.assertLess(elapsed, 6.0)
        self.assert_reaped(result)

    def test_sentinel_char_from_unrelated_output_is_observational(self):
        result = self.run_target(
            self.script("unrelated.py", BODY_FRAME_UNRELATED_SENTINEL),
            probe_input=True,
            probe_char="z",
            probe_timeout=2.0,
        )
        probe = result["input_probe"]
        self.assertEqual(probe["status"], "observed-key-in-output")
        self.assertIsNotNone(probe["ms"])
        self.assertGreaterEqual(probe["ms"], 0.0)
        self.assertLessEqual(probe["ms"], probe["bounded_ms"])
        self.assertIn("bounded post-probe output", probe["definition"])
        self.assertIn("sentinel key", probe["definition"])
        # The char appeared in output without the target reading stdin, so
        # the record must deny consumption rather than infer it.
        self.assert_observational_only(probe["interpretation"])
        self.assertIn("does not prove", probe["interpretation"])
        self.assert_reaped(result)

    def test_tty_echo_of_probe_key_is_observational_not_consumption(self):
        result = self.run_target(
            self.script("echo_driver.py", BODY_FRAME_ECHO_DRIVER),
            probe_input=True,
            probe_char="z",
            probe_timeout=1.0,
            quit_timeout=0.2,
            kill_grace=0.4,
        )
        probe = result["input_probe"]
        self.assertEqual(probe["status"], "observed-key-in-output")
        self.assertIsNotNone(probe["ms"])
        self.assertLessEqual(probe["ms"], probe["bounded_ms"])
        self.assert_observational_only(probe["interpretation"])
        self.assertTrue(result["exit"]["cleanup"]["escalated"])
        self.assert_reaped(result)

    def test_probe_schema_and_wording_hold_for_every_reachable_status(self):
        # One target per reachable probe status. Windows are generous relative
        # to each target's own timing, and the targets that must not see the
        # key enter raw mode before painting, so no status depends on
        # scheduler luck. exited-before-probe additionally needs the "waited
        # child already reaped" precondition, which no real target can stage
        # on this platform; see reaped_after_frame.
        cases = (
            ("observed-key-in-output", 1.0, "echo_driver.py",
             BODY_FRAME_ECHO_DRIVER, {"quit_timeout": 0.2, "kill_grace": 0.4}),
            ("key-not-observed", 0.3, "ignores.py",
             BODY_FRAME_IGNORE_INPUT, {"quit_timeout": 0.2, "kill_grace": 0.4}),
            ("exited-before-key", 2.0, "dies.py",
             BODY_FRAME_EXIT_IN_PROBE_WINDOW, {}),
            ("exited-before-probe", 0.5, "frame_then_exit.py",
             BODY_FRAME_THEN_EXIT, {}),
        )
        seen = []
        for expected, window, name, body, overrides in cases:
            script = self.script(name, body)
            if expected == "exited-before-probe":
                with reaped_after_frame():
                    result = self.run_target(
                        script,
                        probe_input=True,
                        probe_char="z",
                        probe_timeout=window,
                        **overrides,
                    )
            else:
                result = self.run_target(
                    script,
                    probe_input=True,
                    probe_char="z",
                    probe_timeout=window,
                    **overrides,
                )
            # Readiness is a real observation in every case, so the probe
            # block is only reachable once a frame marker was read.
            self.assertEqual(result["ready"]["status"], "observed-frame-ready")
            self.assertEqual(result["ready"]["missing"], [])
            self.assertIsNotNone(result["ready"]["ms"])
            probe = result["input_probe"]
            self.assertIsNotNone(probe)
            seen.append(probe["status"])
            with self.subTest(status=expected):
                self.assertEqual(probe["status"], expected)
                # The stable key set is identical for every status.
                self.assertEqual(PROBE_COMMON_KEYS - set(probe), set())
                self.assertEqual(probe["char"], "z")
                # bounded_ms reports the configured window, never a measured
                # duration, for every status.
                self.assertEqual(probe["bounded_ms"], window * 1000.0)
                # A latency is reported only where the sentinel key was
                # actually seen in bounded post-probe output.
                self.assertEqual(
                    probe["ms"] is not None, expected == "observed-key-in-output"
                )
                if probe["ms"] is not None:
                    self.assertGreaterEqual(probe["ms"], 0.0)
                    self.assertLessEqual(probe["ms"], probe["bounded_ms"])
                # definition is reserved for the status that reports a latency.
                self.assertEqual(
                    "definition" in probe, expected == "observed-key-in-output"
                )
                # No key is written when the target is already gone.
                self.assertEqual(
                    probe["wrote_keys"], expected != "exited-before-probe"
                )
                self.assert_observational_only(probe["interpretation"])
                self.assert_reaped(result)
        self.assertEqual(seen, [case[0] for case in cases])


if __name__ == "__main__":
    unittest.main()