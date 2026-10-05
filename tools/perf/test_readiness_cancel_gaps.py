#!/usr/bin/env python3
"""Cancel-evidence gap tests for tools/perf/readiness.py (stdlib unittest only).

Run from the repository root:
    python3 -m unittest discover -s tools/perf -p 'test_readiness_cancel_gaps.py'
or directly:
    python3 tools/perf/test_readiness_cancel_gaps.py

These tests complement tools/perf/test_readiness.py rather than repeat it.
They pin the honesty rules of the cancellation observation path:

- an unchanged footer redraw (generic text such as "ctrl+c cancel") is never
  cancel evidence, and neither is a state marker that the target printed
  before the cancel key was sent;
- caller-supplied literals/regexes are observational markers, never
  authoritative lifecycle proof;
- an "observed" cancel requires an explicit terminal-state marker;
- the cancel schema keys are stable for every status, and non-cancel paths
  leave the cancel section absent instead of fabricating one.

Every PTY target here is a benign pseudo executable built from the test
process's own Python interpreter plus a generated script; no repository
binary, network, docker, sudo, or cache action is used. All PTY runs use
short explicit timeouts so the suite stays bounded.
"""

import json
import os
import subprocess
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import readiness  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "readiness.py")
POSIX = os.name == "posix" and hasattr(os, "fork") and hasattr(os, "wait4")

# A tiny ratatui-shaped frame whose markers match readiness.DEFAULT_REQUIRED
# on purpose. The footer carries the generic "ctrl+c cancel" hint of the real
# M0-test TUI, so a bare redraw can never satisfy the default evidence set.
FRAME = (
    "\x1b[?1049h"
    "\x1b[1;1H nexus-tui  M0-TEST run:run-1"
    "\x1b[3;1H composer (fixed) "
    "\x1b[5;1H m0-test  ctrl+c cancel  tab focus  ctrl+d quit "
)

# Marker text is emitted at startup, before any cancel key can be sent.
BODY_PRE_CANCEL_MARKER = r'''
import sys, tty
sys.stdout.write(FRAME); sys.stdout.write("Status: Cancelled\r\n"); sys.stdout.flush()
tty.setraw(0)
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b in (b"\x04", b"\x03"):
        break
'''

# First default literal alternative ("run cancelled") instead of the second.
BODY_CANCEL_RUN_CANCELLED = r'''
import sys, tty
sys.stdout.write(FRAME); sys.stdout.flush()
tty.setraw(0)
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b == b"\x04":
        break
    if b == b"\x03":
        sys.stdout.write("Run Cancelled by user\r\n"); sys.stdout.flush()
        break
'''

# Acknowledges cancellation but deliberately stays alive: the marker is
# observable while cancel-to-exit latency is not.
BODY_CANCEL_ACK_STAYS_ALIVE = r'''
import sys, tty
sys.stdout.write(FRAME); sys.stdout.flush()
tty.setraw(0)
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b == b"\x04":
        break
    if b == b"\x03":
        sys.stdout.write("status: cancelled\r\n"); sys.stdout.flush()
'''

# Redraws the identical frame repeatedly after the cancel key: high-volume
# generic footer output, still without any terminal-state marker.
BODY_FOOTER_REDRAW_NOISE = r'''
import sys, tty
sys.stdout.write(FRAME); sys.stdout.flush()
tty.setraw(0)
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b == b"\x04":
        break
    if b == b"\x03":
        for _ in range(5):
            sys.stdout.write(FRAME); sys.stdout.flush()
'''

# Emits nothing except the initial frame and keeps reading: unconfirmed
# because the target exits without observed cancel evidence.
BODY_FRAME_WAIT_QUIT = r'''
import sys, tty
sys.stdout.write(FRAME); sys.stdout.flush()
tty.setraw(0)
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b in (b"\x04", b"\x03"):
        break
'''

BODY_FRAME_CANCEL_ACK = r'''
import sys, tty
sys.stdout.write(FRAME); sys.stdout.flush()
tty.setraw(0)
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b == b"\x04":
        break
    if b == b"\x03":
        sys.stdout.write("Status: Cancelled\r\n"); sys.stdout.flush()
        break
'''

BODY_CANCEL_OUTCOME = r'''
import sys, tty
sys.stdout.write(FRAME); sys.stdout.flush()
tty.setraw(0)
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b == b"\x04":
        break
    if b == b"\x03":
        sys.stdout.write("outcome=cancelled\r\n"); sys.stdout.flush()
        break
'''

BODY_FRAME_THEN_EXIT = r'''
import sys
sys.stdout.write(FRAME); sys.stdout.flush()
'''

BODY_NO_FRAME_SLEEP = r'''
import sys, time
sys.stdout.write("starting up without a frame\r\n"); sys.stdout.flush()
time.sleep(30)
'''

# The exact schema every cancel status must expose (status included).
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


def make_script(directory, name, body):
    path = os.path.join(directory, name)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write("FRAME = " + repr(FRAME) + "\n" + body)
    return path


class EvidenceTrackerGapTests(unittest.TestCase):
    """Marker-level rules behind the cancel evidence decision."""

    def test_default_literals_reject_footer_redraw_text(self):
        tracker = readiness.EvidenceTracker(literals=readiness.DEFAULT_CANCEL_EVIDENCE)
        # The generic footer hint of the frame, exactly as redrawn.
        tracker.feed(b"m0-test  ctrl+c cancel  tab focus  ctrl+d quit")
        self.assertFalse(tracker.complete)
        self.assertEqual(
            tracker.missing(), list(readiness.DEFAULT_CANCEL_EVIDENCE)
        )

    def test_default_literals_accept_either_alternative(self):
        for text in (b"RUN CANCELLED", b"Status: Cancelled", b"status: CANCELLED"):
            with self.subTest(text=text):
                tracker = readiness.EvidenceTracker(
                    literals=readiness.DEFAULT_CANCEL_EVIDENCE
                )
                tracker.feed(text)
                self.assertTrue(tracker.complete)

    def test_any_of_across_literal_and_regex_kinds(self):
        tracker = readiness.EvidenceTracker(
            literals=("run cancelled",), regexes=(r"outcome\s*=\s*cancelled",)
        )
        tracker.feed(b"OUTCOME = CANCELLED")
        self.assertTrue(tracker.complete)
        # The unseen literal is reported; the matched regex is not.
        self.assertEqual(tracker.missing(), ["run cancelled"])

    def test_missing_lists_literals_before_regexes(self):
        tracker = readiness.EvidenceTracker(
            literals=("alpha", "beta"), regexes=(r"gamma\d+",)
        )
        tracker.feed(b"unrelated output")
        self.assertEqual(tracker.missing(), ["alpha", "beta", r"gamma\d+"])

    def test_case_sensitive_mode_does_not_fold_case(self):
        tracker = readiness.EvidenceTracker(
            literals=("Alpha",), case_insensitive=False
        )
        tracker.feed(b"alpha")
        self.assertFalse(tracker.complete)
        tracker.feed(b"Alpha")
        self.assertTrue(tracker.complete)

    def test_seen_keys_are_kind_tagged(self):
        tracker = readiness.EvidenceTracker(
            literals=("alpha",), regexes=(r"b\w+",)
        )
        tracker.feed(b"alpha beta")
        self.assertEqual(
            tracker.seen, {("literal", "alpha"): True, ("regex", r"b\w+"): True}
        )

    def test_empty_marker_set_never_completes(self):
        tracker = readiness.EvidenceTracker()
        tracker.feed(b"status: cancelled")
        self.assertFalse(tracker.complete)
        self.assertEqual(tracker.missing(), [])

    def test_empty_feed_is_a_no_op(self):
        tracker = readiness.EvidenceTracker(literals=("alpha",))
        tracker.feed(b"")
        self.assertFalse(tracker.complete)
        self.assertEqual(tracker.missing(), ["alpha"])

    def test_tail_stays_bounded_after_noise(self):
        tracker = readiness.EvidenceTracker(literals=("alpha",))
        tracker.feed(b"x" * 500000)
        self.assertLessEqual(len(tracker.tail), tracker.tail_limit)
        self.assertLessEqual(tracker.tail_limit, readiness.MARKER_TAIL_BYTES * 4)

    def test_marker_split_across_feeds_still_matches(self):
        tracker = readiness.EvidenceTracker(literals=("status: cancelled",))
        tracker.feed(b"status: cance")
        self.assertFalse(tracker.complete)
        tracker.feed(b"lled")
        self.assertTrue(tracker.complete)


@unittest.skipUnless(POSIX, "PTY harness requires POSIX fork/openpty/wait4")
class CancelGapTestCase(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls._tmp = tempfile.TemporaryDirectory(prefix="nexus-readiness-cancel-")
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

    def assert_observational_only(self, cancel):
        """A reported marker is never authoritative lifecycle proof."""
        self.assertEqual(cancel["status"], "observed")
        self.assertEqual(cancel["latency_claim"], "observational-marker-only")
        self.assertIn("not authoritative", cancel["interpretation"])
        self.assertIn("cannot distinguish", cancel["confound"])
        self.assertIsNone(cancel["reason"])


class FooterRedrawIsNotEvidenceTests(CancelGapTestCase):
    def test_identical_frame_redraw_stays_unconfirmed(self):
        script = self.script("gap_redraw.py", BODY_FOOTER_REDRAW_NOISE)
        started = time.monotonic()
        result = self.run_target(script, mode="cancel", cancel_timeout=0.4)
        elapsed = time.monotonic() - started
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "unconfirmed")
        self.assertIsNone(cancel["cancel_marker_observed_ms"])
        self.assertIsNone(cancel["cancel_to_exit_ms"])
        self.assertIsNone(cancel["latency_claim"])
        self.assertIsNone(cancel["interpretation"])
        self.assertEqual(cancel["evidence_source"], "default")
        self.assertEqual(
            cancel["evidence_literals"], list(readiness.DEFAULT_CANCEL_EVIDENCE)
        )
        self.assertEqual(
            cancel["missing_evidence"], list(readiness.DEFAULT_CANCEL_EVIDENCE)
        )
        self.assertIn("still alive", cancel["reason"])
        self.assertLess(elapsed, 8.0)
        self.assert_reaped(result)

    def test_state_marker_printed_before_cancel_is_not_evidence(self):
        # The target says "Status: Cancelled" at startup and then keeps
        # running. Evidence tracked before the cancel key is sent must not be
        # reused, so this must not be reported as an observed cancel.
        script = self.script("gap_pre_marker.py", BODY_PRE_CANCEL_MARKER)
        result = self.run_target(
            script, mode="cancel", cancel_settle=0.3, cancel_timeout=0.4
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["evidence_source"], "default")
        self.assertEqual(cancel["status"], "unconfirmed")
        self.assertIsNone(cancel["cancel_marker_observed_ms"])
        self.assertIsNone(cancel["latency_claim"])
        self.assertEqual(
            cancel["missing_evidence"], list(readiness.DEFAULT_CANCEL_EVIDENCE)
        )
        self.assert_reaped(result)

    def test_footer_word_alone_does_not_match_default_literals(self):
        for hint in (b"ctrl+c cancel", b"CANCEL", b"cancelled run queued"):
            with self.subTest(hint=hint):
                tracker = readiness.EvidenceTracker(
                    literals=readiness.DEFAULT_CANCEL_EVIDENCE
                )
                tracker.feed(hint)
                self.assertFalse(tracker.complete)


class UserEvidenceIsObservationalTests(CancelGapTestCase):
    def test_user_literal_footer_match_is_labelled_observational(self):
        script = self.script("gap_user_literal.py", BODY_FOOTER_REDRAW_NOISE)
        result = self.run_target(
            script,
            mode="cancel",
            cancel_timeout=0.4,
            cancel_evidence=("cancel",),
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["evidence_source"], "user-supplied")
        self.assertEqual(cancel["evidence_literals"], ["cancel"])
        self.assertEqual(cancel["evidence_regexes"], [])
        self.assertEqual(cancel["missing_evidence"], [])
        self.assert_observational_only(cancel)
        self.assertIsNotNone(cancel["cancel_marker_observed_ms"])
        self.assertGreaterEqual(cancel["cancel_marker_observed_ms"], 0.0)

    def test_user_regex_match_is_labelled_observational(self):
        script = self.script("gap_user_regex.py", BODY_CANCEL_OUTCOME)
        result = self.run_target(
            script,
            mode="cancel",
            cancel_timeout=1.0,
            cancel_evidence=(),
            cancel_evidence_regex=(r"outcome\s*=\s*cancelled",),
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["evidence_source"], "user-supplied-regex")
        self.assertEqual(cancel["evidence_literals"], [])
        self.assertEqual(cancel["evidence_regexes"], [r"outcome\s*=\s*cancelled"])
        self.assertEqual(cancel["missing_evidence"], [])
        self.assert_observational_only(cancel)

    def test_mixed_literal_and_regex_report_mixed_source(self):
        script = self.script("gap_mixed.py", BODY_CANCEL_OUTCOME)
        result = self.run_target(
            script,
            mode="cancel",
            cancel_timeout=1.0,
            cancel_evidence=("outcome=cancelled",),
            cancel_evidence_regex=(r"outcome\s*=\s*cancelled",),
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["evidence_source"], "user-supplied-mixed")
        self.assert_observational_only(cancel)

    def test_regex_without_match_is_unconfirmed_and_lists_pattern(self):
        script = self.script("gap_regex_nomatch.py", BODY_FRAME_CANCEL_ACK)
        result = self.run_target(
            script,
            mode="cancel",
            cancel_timeout=0.4,
            cancel_evidence=(),
            cancel_evidence_regex=(r"aborted by operator",),
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "unconfirmed")
        self.assertEqual(cancel["evidence_source"], "user-supplied-regex")
        self.assertEqual(cancel["missing_evidence"], [r"aborted by operator"])
        self.assertIsNone(cancel["cancel_marker_observed_ms"])
        self.assertIsNone(cancel["cancel_to_exit_ms"])
        self.assert_reaped(result)

    def test_no_evidence_markers_configured_stays_unconfirmed(self):
        # A caller can suppress evidence entirely; nothing may be reported.
        script = self.script("gap_no_evidence.py", BODY_FRAME_CANCEL_ACK)
        result = self.run_target(
            script,
            mode="cancel",
            cancel_timeout=0.4,
            cancel_evidence=(),
            cancel_evidence_regex=(),
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "unconfirmed")
        self.assertEqual(cancel["evidence_literals"], [])
        self.assertEqual(cancel["evidence_regexes"], [])
        self.assertEqual(cancel["missing_evidence"], [])
        self.assertIsNone(cancel["cancel_marker_observed_ms"])
        self.assertIsNone(cancel["latency_claim"])
        self.assert_reaped(result)


class ExplicitStateMarkerTests(CancelGapTestCase):
    def test_run_cancelled_literal_is_observed(self):
        script = self.script("gap_run_cancelled.py", BODY_CANCEL_RUN_CANCELLED)
        result = self.run_target(script, mode="cancel", cancel_timeout=1.0)
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "observed")
        self.assertIsNotNone(cancel["cancel_marker_observed_ms"])
        self.assertGreaterEqual(cancel["cancel_marker_observed_ms"], 0.0)
        self.assertEqual(cancel["evidence_source"], "default")
        self.assertEqual(
            cancel["evidence_literals"], list(readiness.DEFAULT_CANCEL_EVIDENCE)
        )
        # Evidence is any-of, so only the matched alternative ("run cancelled")
        # is seen; the other default literal stays in missing_evidence.
        self.assertEqual(
            cancel["missing_evidence"], list(readiness.DEFAULT_CANCEL_EVIDENCE)[1:]
        )
        self.assertTrue(cancel["wrote_keys"])
        self.assert_observational_only(cancel)

    def test_observed_marker_without_exit_reports_no_exit_latency(self):
        script = self.script("gap_ack_alive.py", BODY_CANCEL_ACK_STAYS_ALIVE)
        result = self.run_target(script, mode="cancel", cancel_timeout=0.5)
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "observed")
        self.assertIsNotNone(cancel["cancel_marker_observed_ms"])
        self.assertIsNone(cancel["cancel_to_exit_ms"])
        self.assertFalse(cancel["exited_within_window"])
        self.assert_observational_only(cancel)
        self.assert_reaped(result)

    def test_observed_marker_with_exit_reports_both_latencies(self):
        script = self.script("gap_ack_exit.py", BODY_FRAME_CANCEL_ACK)
        result = self.run_target(script, mode="cancel", cancel_timeout=1.0)
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "observed")
        self.assertTrue(cancel["exited_within_window"])
        self.assertIsNotNone(cancel["cancel_to_exit_ms"])
        self.assertGreaterEqual(cancel["cancel_to_exit_ms"], 0.0)
        self.assert_observational_only(cancel)

    def test_literal_and_regex_need_only_one_to_match(self):
        script = self.script("gap_any_of.py", BODY_FRAME_CANCEL_ACK)
        result = self.run_target(
            script,
            mode="cancel",
            cancel_timeout=1.0,
            cancel_evidence=("never printed",),
            cancel_evidence_regex=(r"status:\s*cancelled",),
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "observed")
        self.assertEqual(cancel["evidence_source"], "user-supplied-mixed")
        self.assertEqual(cancel["missing_evidence"], ["never printed"])


class CancelSchemaTests(CancelGapTestCase):
    def _cancel_run(self, name, body, **overrides):
        return self.run_target(
            self.script(name, body), mode="cancel", cancel_timeout=0.6, **overrides
        )

    def test_schema_keys_are_stable_across_all_four_statuses(self):
        runs = {
            "skipped": self._cancel_run(
                "gap_schema_exits.py", BODY_FRAME_THEN_EXIT, cancel_settle=0.3
            ),
            "unconfirmed_alive": self._cancel_run(
                "gap_schema_alive.py", BODY_FOOTER_REDRAW_NOISE
            ),
            "unconfirmed_exit": self._cancel_run(
                "gap_schema_noexit.py", BODY_FRAME_WAIT_QUIT
            ),
            "observed": self._cancel_run(
                "gap_schema_ack.py", BODY_FRAME_CANCEL_ACK
            ),
        }
        observed_statuses = []
        for label, result in runs.items():
            with self.subTest(run=label):
                cancel = result["cancel"]
                self.assertIsNotNone(cancel)
                self.assertEqual(set(cancel), CANCEL_KEYS)
                observed_statuses.append(cancel["status"])
            self.assert_reaped(result)
        self.assertEqual(
            sorted(observed_statuses),
            ["observed", "skipped", "unconfirmed", "unconfirmed"],
        )

    def test_skipped_path_leaves_measurements_unset(self):
        result = self._cancel_run(
            "gap_skip.py", BODY_FRAME_THEN_EXIT, cancel_settle=0.3
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "skipped")
        self.assertIn("finished", cancel["reason"])
        self.assertTrue(cancel["exited_within_window"])
        self.assertIsNone(cancel["cancel_marker_observed_ms"])
        self.assertIsNone(cancel["cancel_to_exit_ms"])
        self.assertIsNone(cancel["wrote_keys"])
        self.assertEqual(cancel["missing_evidence"], [])
        self.assertIsNone(cancel["latency_claim"])
        self.assertIsNone(cancel["interpretation"])
        self.assertEqual(cancel["evidence_source"], "default")

    def test_unconfirmed_after_exit_has_no_latency_or_claim(self):
        result = self._cancel_run("gap_unconf_exit.py", BODY_FRAME_WAIT_QUIT)
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "unconfirmed")
        self.assertTrue(cancel["exited_within_window"])
        self.assertIsNone(cancel["cancel_marker_observed_ms"])
        self.assertIsNone(cancel["cancel_to_exit_ms"])
        self.assertIsNone(cancel["latency_claim"])
        self.assertIsNone(cancel["confound"])
        self.assertIn("without observed cancel evidence", cancel["reason"])

    def test_no_cancel_section_when_readiness_fails(self):
        script = self.script("gap_no_frame.py", BODY_NO_FRAME_SLEEP)
        result = self.run_target(
            script,
            mode="cancel",
            ready_timeout=0.3,
            timeout=5.0,
            quit_timeout=0.3,
            kill_grace=0.3,
        )
        self.assertEqual(result["ready"]["status"], "ready-timeout")
        self.assertIsNone(result["cancel"])
        self.assertEqual(result["outcome"], "ready-timeout")
        self.assert_reaped(result)

    def test_no_cancel_section_outside_cancel_mode(self):
        result = self.run_target(
            self.script("gap_ready_mode.py", BODY_FRAME_CANCEL_ACK), mode="ready"
        )
        self.assertEqual(result["ready"]["status"], "observed-frame-ready")
        self.assertIsNone(result["cancel"])
        self.assert_reaped(result)

    def test_spawn_error_leaves_no_cancel_section(self):
        # An empty argv is rejected by spawn_pty, which is the deterministic
        # way to reach the spawn-error branch.
        result = readiness.run_measurement(readiness.RunConfig(mode="cancel"), [], 0)
        self.assertEqual(result["outcome"], "spawn-error")
        self.assertEqual(result["spawn"]["status"], "error")
        self.assertIsNone(result["ready"])
        self.assertIsNone(result["cancel"])

    def test_exec_failure_leaves_no_cancel_section(self):
        # A non-executable path: the child exits 127, so no cancel observation
        # may be reported and no readiness latency may be invented.
        result = readiness.run_measurement(
            readiness.RunConfig(mode="cancel"),
            [os.path.join(self.tmp, "does-not-exist-")],
            0,
        )
        self.assertIsNone(result["ready"]["ms"])
        self.assertIsNone(result["cancel"])
        self.assertEqual(result["exit"]["exit_code"], 127)
        self.assert_reaped(result)


class CancelSummaryTests(unittest.TestCase):
    """Pure aggregation checks; no PTY needed."""

    def _run(self, index, cancel):
        return {
            "run_index": index,
            "ready": {"status": "observed-frame-ready", "ms": 1.0},
            "cancel": cancel,
            "exit": {"cleanup": {"escalated": False}},
            "outcome": "ok",
        }

    def test_summary_counts_and_collects_only_observed_markers(self):
        cfg = readiness.RunConfig(mode="cancel")
        runs = [
            self._run(
                0,
                {
                    "status": "observed",
                    "cancel_marker_observed_ms": 12.0,
                    "cancel_to_exit_ms": 20.0,
                },
            ),
            self._run(
                1,
                {
                    "status": "observed",
                    "cancel_marker_observed_ms": 8.0,
                    "cancel_to_exit_ms": None,
                },
            ),
            self._run(2, {"status": "unconfirmed", "cancel_marker_observed_ms": None}),
            self._run(3, {"status": "skipped", "cancel_marker_observed_ms": None}),
        ]
        summary = readiness.summarize_runs(cfg, runs)
        cancel = summary["cancel"]
        self.assertEqual(summary["runs"], 4)
        self.assertEqual(cancel["observed"], 2)
        self.assertEqual(cancel["skipped"], 1)
        self.assertEqual(cancel["unconfirmed"], 1)
        self.assertEqual(cancel["observed_marker_ms"], [12.0, 8.0])
        self.assertEqual(cancel["observed_to_exit_ms"], [20.0])
        self.assertEqual(summary["cleanup_escalations"], 0)

    def test_summary_omits_cancel_section_for_other_modes(self):
        cfg = readiness.RunConfig(mode="ready")
        runs = [
            {
                "run_index": 0,
                "ready": {"status": "observed-frame-ready", "ms": 3.0},
                "cancel": None,
                "exit": {"cleanup": {"escalated": True}},
                "outcome": "ok",
            }
        ]
        summary = readiness.summarize_runs(cfg, runs)
        self.assertNotIn("cancel", summary)
        self.assertNotIn("idle", summary)
        self.assertEqual(summary["cleanup_escalations"], 1)
        self.assertEqual(summary["ready_ms"]["n"], 1)


class CancelCliEvidenceTests(CancelGapTestCase):
    def test_cli_records_evidence_config_and_unconfirmed_status(self):
        script = self.script("gap_cli.py", BODY_FOOTER_REDRAW_NOISE)
        json_path = os.path.join(self.tmp, "gap_cli.json")
        completed = subprocess.run(
            [
                sys.executable,
                SCRIPT,
                sys.executable,
                "--mode",
                "cancel",
                "--runs",
                "1",
                "--require",
                "composer (fixed)",
                "--require",
                "m0-test",
                "--cancel-timeout",
                "0.4",
                "--cancel-settle",
                "0.2",
                "--json",
                json_path,
                "--",
                "-u",
                script,
            ],
            capture_output=True,
            text=True,
            timeout=60,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        with open(json_path, encoding="utf-8") as handle:
            record = json.load(handle)
        self.assertEqual(record["schema"], readiness.SCHEMA)
        self.assertEqual(
            record["config"]["cancel_evidence"], list(readiness.DEFAULT_CANCEL_EVIDENCE)
        )
        self.assertEqual(record["config"]["cancel_evidence_regex"], [])
        cancel = record["runs"][0]["cancel"]
        self.assertEqual(set(cancel), CANCEL_KEYS)
        self.assertEqual(cancel["status"], "unconfirmed")
        self.assertIsNone(cancel["cancel_marker_observed_ms"])
        self.assertIn("evidence_source=default", completed.stdout)

    def test_cli_regex_only_config_replaces_default_literals(self):
        script = self.script("gap_cli_regex.py", BODY_CANCEL_OUTCOME)
        json_path = os.path.join(self.tmp, "gap_cli_regex.json")
        completed = subprocess.run(
            [
                sys.executable,
                SCRIPT,
                sys.executable,
                "--mode",
                "cancel",
                "--runs",
                "1",
                "--require",
                "composer (fixed)",
                "--require",
                "m0-test",
                "--cancel-timeout",
                "1.0",
                "--cancel-evidence-regex",
                r"outcome\s*=\s*cancelled",
                "--json",
                json_path,
                "--",
                "-u",
                script,
            ],
            capture_output=True,
            text=True,
            timeout=60,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        with open(json_path, encoding="utf-8") as handle:
            record = json.load(handle)
        self.assertEqual(record["config"]["cancel_evidence"], [])
        self.assertEqual(
            record["config"]["cancel_evidence_regex"], [r"outcome\s*=\s*cancelled"]
        )
        cancel = record["runs"][0]["cancel"]
        self.assertEqual(cancel["status"], "observed")
        self.assertEqual(cancel["evidence_source"], "user-supplied-regex")
        self.assertEqual(record["summary"]["cancel"]["observed"], 1)
        self.assertIn("evidence_source=user-supplied-regex", completed.stdout)


if __name__ == "__main__":
    unittest.main()