#!/usr/bin/env python3
"""Regression tests for tools/perf/readiness.py (stdlib unittest only).

Run from the repository root:
    python3 -m unittest discover -s tools/perf -p 'test_*.py'
or directly:
    python3 tools/perf/test_readiness.py

Every PTY target here is a benign pseudo executable built from the test
process's own Python interpreter plus a generated script; no repository
binary, network, docker, sudo, or cache action is used.
"""

import hashlib
import json
import os
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import readiness  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "readiness.py")
POSIX = os.name == "posix" and hasattr(os, "fork") and hasattr(os, "wait4")

# A tiny ratatui-shaped frame: alternate screen, header, fixed composer,
# footer. Markers match readiness.DEFAULT_REQUIRED on purpose, and the
# footer intentionally contains "ctrl+c cancel" like the real M0-test TUI
# so a mere footer redraw can never satisfy default cancel evidence.
FRAME = (
    "\x1b[?1049h"
    "\x1b[1;1H nexus-tui  M0-TEST run:run-1"
    "\x1b[3;1H composer (fixed) "
    "\x1b[5;1H m0-test  ctrl+c cancel  tab focus  ctrl+d quit "
)

BODY_FRAME_WAIT_QUIT = r'''
import sys, tty
tty.setraw(0)
sys.stdout.write(FRAME); sys.stdout.flush()
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b in (b"\x04", b"\x03"):
        break
'''

BODY_FRAME_CANCEL_ACK = r'''
import sys, tty
tty.setraw(0)
sys.stdout.write(FRAME); sys.stdout.flush()
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b == b"\x04":
        break
    if b == b"\x03":
        sys.stdout.write("Status: Cancelled\r\n"); sys.stdout.flush()
        break
'''

BODY_FRAME_CANCEL_OUTCOME = r'''
import sys, tty
tty.setraw(0)
sys.stdout.write(FRAME); sys.stdout.flush()
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b == b"\x04":
        break
    if b == b"\x03":
        sys.stdout.write("outcome=cancelled\r\n"); sys.stdout.flush()
        break
'''

BODY_FRAME_FOOTER_REDRAW = r'''
import sys, tty
tty.setraw(0)
sys.stdout.write(FRAME); sys.stdout.flush()
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b == b"\x04":
        break
    if b == b"\x03":
        sys.stdout.write(FRAME); sys.stdout.flush()
'''

BODY_FRAME_ECHO = r'''
import sys, tty
tty.setraw(0)
sys.stdout.write(FRAME); sys.stdout.flush()
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b in (b"\x04", b"\x03"):
        break
    sys.stdout.write("echo:" + b.decode("utf-8", "replace") + "\r\n")
    sys.stdout.flush()
'''

BODY_FRAME_THEN_EXIT = r'''
import sys
sys.stdout.write(FRAME); sys.stdout.flush()
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

BODY_FRAME_SIGTERM_IGNORE = r'''
import signal, sys, time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
sys.stdout.write(FRAME); sys.stdout.flush()
while True:
    time.sleep(0.1)
'''

BODY_FRAME_BUSY_THEN_QUIT = r'''
import sys, time, tty
sys.stdout.write(FRAME); sys.stdout.flush()
deadline = time.monotonic() + 0.4
while time.monotonic() < deadline:
    pass
tty.setraw(0)
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b in (b"\x04", b"\x03"):
        break
'''

BODY_FRAME_BIG_OUTPUT = r'''
import sys, tty
sys.stdout.write("x" * 200000)
tty.setraw(0)
sys.stdout.write(FRAME); sys.stdout.flush()
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b in (b"\x04", b"\x03"):
        break
'''

BODY_FRAME_RSS = r'''
import sys, tty
ballast = bytearray(8 << 20)
ballast[0] = 1
tty.setraw(0)
sys.stdout.write(FRAME); sys.stdout.flush()
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b in (b"\x04", b"\x03"):
        break
'''


def make_script(directory, name, body):
    path = os.path.join(directory, name)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write("FRAME = " + repr(FRAME) + "\n" + body)
    return path


class LocalStatsTests(unittest.TestCase):
    def test_percentile_empty_is_none(self):
        self.assertIsNone(readiness.percentile_nearest_rank([], 50))

    def test_percentile_nearest_rank(self):
        self.assertEqual(readiness.percentile_nearest_rank([1, 2, 3, 4], 50), 2.0)
        self.assertEqual(readiness.percentile_nearest_rank([1, 2], 95), 2.0)
        self.assertEqual(readiness.percentile_nearest_rank([7.0], 95), 7.0)

    def test_summarize_ms(self):
        self.assertEqual(
            readiness.summarize_ms([]),
            {"n": 0, "p50_ms": None, "p95_ms": None, "max_ms": None},
        )
        stats = readiness.summarize_ms([3.0, 1.0, 2.0])
        self.assertEqual(stats["n"], 3)
        self.assertEqual(stats["p50_ms"], 2.0)
        self.assertEqual(stats["max_ms"], 3.0)

    def test_default_markers_document_the_observed_frame_rule(self):
        self.assertEqual(readiness.DEFAULT_REQUIRED, ("composer (fixed)", "m0-test"))


class AnsiFilterTests(unittest.TestCase):
    def test_strips_csi_sequences(self):
        ansi = readiness.AnsiFilter()
        self.assertEqual(ansi.feed(b"\x1b[31mred\x1b[0m"), b"red")

    def test_strips_osc_sequences_bel_and_st(self):
        ansi = readiness.AnsiFilter()
        self.assertEqual(ansi.feed(b"\x1b]0;title\x07text"), b"text")
        ansi = readiness.AnsiFilter()
        self.assertEqual(ansi.feed(b"\x1b]0;title\x1b\\text"), b"text")

    def test_sequences_split_across_feeds(self):
        ansi = readiness.AnsiFilter()
        self.assertEqual(ansi.feed(b"\x1b[3"), b"")
        self.assertEqual(ansi.feed(b"1mred"), b"red")

    def test_keeps_utf8_and_drops_other_controls(self):
        ansi = readiness.AnsiFilter()
        out = ansi.feed(b"a\x07b\x08c\n\td\xe2\x94\x8c")
        self.assertEqual(out, b"abc\n\td\xe2\x94\x8c")


class CaptureBufferTests(unittest.TestCase):
    def test_bounded_ring_counts_dropped_bytes(self):
        capture = readiness.CaptureBuffer(10)
        capture.feed(b"abcdefgh")
        capture.feed(b"ijklmnop")
        self.assertEqual(capture.total_bytes, 16)
        self.assertEqual(capture.dropped_bytes, 6)
        self.assertEqual(bytes(capture.buffer), b"ghijklmnop")

    def test_zero_limit_drops_everything(self):
        capture = readiness.CaptureBuffer(0)
        capture.feed(b"abc")
        self.assertEqual(capture.dropped_bytes, 3)
        self.assertEqual(len(capture.buffer), 0)


class MarkerTrackerTests(unittest.TestCase):
    def test_detects_marker_split_across_feeds(self):
        tracker = readiness.MarkerTracker(("needle",))
        tracker.feed(b"nee")
        self.assertFalse(tracker.complete)
        tracker.feed(b"dle")
        self.assertTrue(tracker.complete)

    def test_missing_lists_unseen_markers(self):
        tracker = readiness.MarkerTracker(("a", "b"))
        tracker.feed(b"a")
        self.assertEqual(tracker.missing(), ["b"])

    def test_remembers_marker_after_tail_rolls(self):
        tracker = readiness.MarkerTracker(("needle",))
        tracker.feed(b"needle")
        tracker.feed(b"x" * 100000)
        self.assertTrue(tracker.complete)


class EvidenceTrackerTests(unittest.TestCase):
    def test_default_literals_are_case_insensitive_and_any_of(self):
        tracker = readiness.EvidenceTracker(
            literals=("run cancelled", "status: cancelled")
        )
        tracker.feed(b"RUN CANCELLED")
        self.assertTrue(tracker.complete)

    def test_generic_footer_text_does_not_match_defaults(self):
        tracker = readiness.EvidenceTracker(
            literals=("run cancelled", "status: cancelled")
        )
        tracker.feed(b"m0-test  ctrl+c cancel  tab focus  ctrl+d quit")
        self.assertFalse(tracker.complete)
        self.assertEqual(len(tracker.missing()), 2)

    def test_user_regex_matches_case_insensitively(self):
        tracker = readiness.EvidenceTracker(regexes=(r"outcome\s*=\s*cancelled",))
        tracker.feed(b"OUTCOME = CANCELLED")
        self.assertTrue(tracker.complete)

    def test_no_match_stays_incomplete(self):
        tracker = readiness.EvidenceTracker(literals=("status: cancelled",))
        tracker.feed(b"status: running")
        self.assertFalse(tracker.complete)
        self.assertEqual(tracker.missing(), ["status: cancelled"])


@unittest.skipUnless(POSIX, "PTY harness requires POSIX fork/openpty/wait4")
class PtyTestCase(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls._tmp = tempfile.TemporaryDirectory(prefix="nexus-readiness-")
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


class FrameReadinessTests(PtyTestCase):
    def test_observed_frame_ready_and_wait4_rss(self):
        script = self.script("frame.py", BODY_FRAME_WAIT_QUIT)
        result = self.run_target(script)
        self.assertEqual(result["outcome"], "ok")
        self.assertEqual(result["ready"]["status"], "observed-frame-ready")
        self.assertIsNotNone(result["ready"]["ms"])
        self.assertGreaterEqual(result["ready"]["ms"], 0.0)
        self.assertTrue(result["ready"]["header_seen"])
        self.assertTrue(result["ready"]["alt_screen_seen"])
        self.assertEqual(result["ready"]["missing"], [])
        self.assertEqual(result["exit"]["status"], "exited")
        self.assertEqual(result["exit"]["exit_code"], 0)
        self.assertEqual(result["exit"]["cleanup"]["method"], "quit-keys")
        self.assertFalse(result["exit"]["cleanup"]["escalated"])
        self.assertIsNotNone(result["resources"]["ru_maxrss"])
        self.assertGreater(result["resources"]["ru_maxrss"], 0)
        self.assert_reaped(result)

    def test_ready_timeout_is_bounded_and_cleans_up(self):
        script = self.script("no_frame.py", BODY_NO_FRAME_SLEEP)
        started = readiness.time.monotonic()
        result = self.run_target(
            script, ready_timeout=0.3, timeout=5.0, quit_timeout=0.3, kill_grace=0.3
        )
        elapsed = readiness.time.monotonic() - started
        self.assertEqual(result["ready"]["status"], "ready-timeout")
        self.assertEqual(result["outcome"], "ready-timeout")
        self.assertLess(elapsed, 5.0)
        self.assertEqual(result["exit"]["status"], "exited")
        self.assertTrue(result["exit"]["cleanup"]["escalated"])
        self.assert_reaped(result)

    def test_frame_before_exit_is_observed_not_fabricated(self):
        script = self.script("exits.py", BODY_FRAME_THEN_EXIT)
        result = self.run_target(script)
        self.assertEqual(result["ready"]["status"], "observed-frame-ready")
        self.assertEqual(result["outcome"], "ok")
        self.assert_reaped(result)

    def test_exit_without_frame_is_reported_not_fabricated(self):
        script = self.script("silent_exit.py", BODY_NO_FRAME_EXIT)
        result = self.run_target(script)
        self.assertEqual(result["ready"]["status"], "exited-before-ready")
        self.assertIsNone(result["ready"]["ms"])
        self.assertEqual(result["outcome"], "ok")
        self.assert_reaped(result)

    def test_capture_is_bounded_with_large_output(self):
        script = self.script("big.py", BODY_FRAME_BIG_OUTPUT)
        result = self.run_target(script, max_capture_bytes=4096)
        self.assertEqual(result["ready"]["status"], "observed-frame-ready")
        self.assertGreater(result["capture"]["dropped_bytes"], 0)
        self.assertLessEqual(result["capture"]["retained_bytes"], 4096)
        self.assertGreater(result["capture"]["total_bytes"], 200000)

    def test_custom_required_markers_override_defaults(self):
        script = self.script("frame.py", BODY_FRAME_WAIT_QUIT)
        result = self.run_target(script, required=("composer (fixed)",))
        self.assertEqual(result["ready"]["status"], "observed-frame-ready")
        self.assertEqual(result["ready"]["required"], ["composer (fixed)"])


class CleanupTests(PtyTestCase):
    def test_sigterm_escalation_when_quit_keys_ignored(self):
        script = self.script("no_frame.py", BODY_NO_FRAME_SLEEP)
        result = self.run_target(
            script, ready_timeout=0.3, timeout=5.0, quit_timeout=0.3, kill_grace=0.5
        )
        self.assertEqual(result["exit"]["cleanup"]["method"], "sigterm")
        self.assertTrue(result["exit"]["cleanup"]["escalated"])
        self.assert_reaped(result)

    def test_sigkill_escalation_when_sigterm_ignored(self):
        script = self.script("stubborn.py", BODY_FRAME_SIGTERM_IGNORE)
        result = self.run_target(script, quit_timeout=0.2, kill_grace=0.3)
        self.assertEqual(result["exit"]["cleanup"]["method"], "sigkill")
        self.assertTrue(result["exit"]["cleanup"]["escalated"])
        self.assert_reaped(result)


class InputProbeTests(PtyTestCase):
    def test_probe_key_in_output_is_observational(self):
        script = self.script("echo.py", BODY_FRAME_ECHO)
        result = self.run_target(
            script, probe_input=True, probe_char="z", probe_timeout=2.0
        )
        probe = result["input_probe"]
        self.assertEqual(probe["status"], "observed-key-in-output")
        self.assertIsNotNone(probe["ms"])
        self.assertGreaterEqual(probe["ms"], 0.0)
        self.assertIn("observational", probe["interpretation"])

    def test_probe_key_not_observed_when_target_ignores_input(self):
        script = self.script("frame.py", BODY_FRAME_WAIT_QUIT)
        result = self.run_target(
            script, probe_input=True, probe_char="z", probe_timeout=0.3
        )
        probe = result["input_probe"]
        self.assertEqual(probe["status"], "key-not-observed")
        self.assertIsNone(probe["ms"])
        self.assertIn("observational", probe["interpretation"])


CANCEL_KEYS = {
    "status",
    "reason",
    "cancel_marker_observed_ms",
    "cancel_to_exit_ms",
    "wrote_keys",
    "evidence_source",
    "evidence_literals",
    "evidence_regexes",
    "missing_evidence",
    "exited_within_window",
    "interpretation",
    "latency_claim",
    "confound",
}


class CancelTests(PtyTestCase):
    def test_cancel_schema_keys_are_stable_for_every_status(self):
        skipped = self.run_target(
            self.script("exits.py", BODY_FRAME_THEN_EXIT),
            mode="cancel",
            cancel_settle=0.2,
        )
        unconfirmed = self.run_target(
            self.script("frame.py", BODY_FRAME_WAIT_QUIT),
            mode="cancel",
            cancel_timeout=0.3,
        )
        observed = self.run_target(
            self.script("cancel_ack.py", BODY_FRAME_CANCEL_ACK),
            mode="cancel",
            cancel_timeout=1.0,
        )
        statuses = []
        for result in (skipped, unconfirmed, observed):
            statuses.append(result["cancel"]["status"])
            with self.subTest(status=result["cancel"]["status"]):
                self.assertEqual(set(result["cancel"]), CANCEL_KEYS)
        self.assertEqual(statuses, ["skipped", "unconfirmed", "observed"])

    def test_cancel_skipped_when_target_finished(self):
        script = self.script("exits.py", BODY_FRAME_THEN_EXIT)
        result = self.run_target(script, mode="cancel", cancel_settle=0.2)
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "skipped")
        self.assertIsNone(cancel["cancel_marker_observed_ms"])
        self.assertIsNone(cancel["cancel_to_exit_ms"])
        self.assertIn("finished", cancel["reason"])

    def test_cancel_observed_requires_explicit_state_marker(self):
        script = self.script("cancel_ack.py", BODY_FRAME_CANCEL_ACK)
        result = self.run_target(script, mode="cancel", cancel_timeout=3.0)
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "observed")
        self.assertIsNotNone(cancel["cancel_marker_observed_ms"])
        self.assertGreaterEqual(cancel["cancel_marker_observed_ms"], 0.0)
        self.assertEqual(cancel["evidence_source"], "default")
        self.assertEqual(cancel["latency_claim"], "observational-marker-only")
        self.assertIn("not authoritative", cancel["interpretation"])

    def test_cancel_unconfirmed_when_target_exits_without_evidence(self):
        script = self.script("frame.py", BODY_FRAME_WAIT_QUIT)
        result = self.run_target(script, mode="cancel", cancel_timeout=3.0)
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "unconfirmed")
        self.assertIsNone(cancel["cancel_marker_observed_ms"])
        self.assertIsNone(cancel["cancel_to_exit_ms"])
        self.assertIn("without observed cancel evidence", cancel["reason"])

    def test_unchanged_footer_redraw_is_not_cancel_evidence(self):
        script = self.script("footer_redraw.py", BODY_FRAME_FOOTER_REDRAW)
        result = self.run_target(script, mode="cancel", cancel_timeout=0.5)
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "unconfirmed")
        self.assertIsNone(cancel["cancel_marker_observed_ms"])
        self.assertEqual(cancel["evidence_source"], "default")
        self.assertIn("still alive", cancel["reason"])

    def test_user_literal_evidence_is_observational(self):
        script = self.script("footer_redraw.py", BODY_FRAME_FOOTER_REDRAW)
        result = self.run_target(
            script,
            mode="cancel",
            cancel_timeout=0.5,
            cancel_evidence=("cancel",),
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "observed")
        self.assertIsNotNone(cancel["cancel_marker_observed_ms"])
        self.assertEqual(cancel["evidence_source"], "user-supplied")
        self.assertEqual(cancel["latency_claim"], "observational-marker-only")

    def test_user_regex_evidence_is_observational(self):
        script = self.script("cancel_outcome.py", BODY_FRAME_CANCEL_OUTCOME)
        result = self.run_target(
            script,
            mode="cancel",
            cancel_timeout=1.0,
            cancel_evidence=(),
            cancel_evidence_regex=("outcome=cancelled",),
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "observed")
        self.assertIsNotNone(cancel["cancel_marker_observed_ms"])
        self.assertEqual(cancel["evidence_source"], "user-supplied-regex")


class IdleTests(PtyTestCase):
    def test_idle_cpu_and_rss_sampling_with_unsupported_wakeups(self):
        script = self.script("busy.py", BODY_FRAME_BUSY_THEN_QUIT)
        result = self.run_target(
            script, mode="idle", idle_window=0.6, idle_interval=0.1
        )
        idle = result["idle"]
        self.assertIn(idle["status"], ("observed", "short"))
        self.assertIsNotNone(idle["cpu_seconds"])
        self.assertGreater(idle["cpu_seconds"], 0.05)
        self.assertIsNotNone(idle["idle_cpu_percent"])
        self.assertGreaterEqual(idle["idle_cpu_percent"], 0.0)
        self.assertEqual(idle["wakeups"]["status"], "unsupported")
        self.assertGreater(result["resources"]["ru_maxrss"], 0)

    def test_rss_high_water_from_wait4(self):
        script = self.script("rss.py", BODY_FRAME_RSS)
        result = self.run_target(script)
        self.assertIsNotNone(result["resources"]["ru_maxrss"])
        self.assertGreater(result["resources"]["ru_maxrss"], 0)
        self.assertIn("ru_maxrss", result["resources"]["ru_maxrss_unit"])


class MethodologyTests(PtyTestCase):
    def test_collect_methodology_records_provenance(self):
        script = self.script("probe.py", BODY_FRAME_THEN_EXIT)
        command = [sys.executable, "-u", script]
        info = readiness.collect_methodology(
            script, command, "unit-test", {"suite": "unit"}, ("m",), 80, 24
        )
        with open(script, "rb") as handle:
            digest = hashlib.sha256(handle.read()).hexdigest()
        self.assertEqual(info["schema"], readiness.SCHEMA)
        self.assertEqual(info["binary_sha256"], digest)
        self.assertEqual(info["build_profile"], "unit-test")
        self.assertEqual(info["command"], command)
        self.assertEqual(info["user_supplied"], {"suite": "unit"})
        self.assertEqual(info["cache_state"], "warm")
        self.assertIn("system", info["platform"])
        self.assertIn("sysname", info["uname"])
        self.assertIn("rustc", info["compiler"])
        self.assertIn("source_commit", info)
        self.assertIn("lockfile_sha256", info)

    def test_cli_json_evidence_end_to_end(self):
        script = self.script("cli_target.py", BODY_FRAME_WAIT_QUIT)
        json_path = os.path.join(self.tmp, "out.json")
        completed = subprocess.run(
            [
                sys.executable,
                SCRIPT,
                sys.executable,
                "--mode",
                "ready",
                "--runs",
                "1",
                "--require",
                "composer (fixed)",
                "--json",
                json_path,
                "--label",
                "suite=unit",
                "--build-profile",
                "unit-test",
                "--",
                "-u",
                script,
            ],
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        with open(json_path, encoding="utf-8") as handle:
            record = json.load(handle)
        self.assertEqual(record["schema"], readiness.SCHEMA)
        self.assertEqual(record["methodology"]["build_profile"], "unit-test")
        self.assertEqual(record["methodology"]["user_supplied"], {"suite": "unit"})
        self.assertEqual(record["runs"][0]["ready"]["status"], "observed-frame-ready")
        self.assertEqual(record["summary"]["ready_ms"]["n"], 1)

    def test_cli_help_and_usage_error(self):
        helped = subprocess.run(
            [sys.executable, SCRIPT, "--help"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(helped.returncode, 0)
        self.assertIn("usage", helped.stdout.lower())
        bad = subprocess.run(
            [
                sys.executable,
                SCRIPT,
                sys.executable,
                "--mode",
                "cancel",
                "--probe-input",
            ],
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(bad.returncode, 2)


if __name__ == "__main__":
    unittest.main()
