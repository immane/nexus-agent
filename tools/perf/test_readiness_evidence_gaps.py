#!/usr/bin/env python3
"""Evidence-schema gap tests for tools/perf/readiness.py (stdlib unittest only).

Companion to test_readiness.py. That file pins the behavioural happy paths;
this one pins the *shape* of the evidence document: the exact key set of every
status-bearing block, the closed set of ``evidence_source`` values, the rule
that ``latency_claim`` is either absent-equivalent (None) or the single
observational sentinel, when ``confound``/``interpretation`` may be filled in,
and how ``missing_evidence`` reports alternatives that were not seen.

Run from the repository root:
    python3 -m unittest discover -s tools/perf -p 'test_*.py'
or directly:
    python3 tools/perf/test_readiness_evidence_gaps.py

Deterministic and bounded: every target is this interpreter plus a generated
script under a temporary directory; no network, docker, sudo, cache action, or
repository binary is used. Timeouts are short so the suite cannot hang.
"""

import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import readiness  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
POSIX = os.name == "posix" and hasattr(os, "fork") and hasattr(os, "wait4")

# Same frame as test_readiness.py: alternate screen, header, fixed composer,
# footer. The footer carries "ctrl+c cancel" so a plain redraw can never
# satisfy the default cancel-evidence literals.
FRAME = (
    "\x1b[?1049h"
    "\x1b[1;1H nexus-tui  M0-TEST run:run-1"
    "\x1b[3;1H composer (fixed) "
    "\x1b[5;1H m0-test  ctrl+c cancel  tab focus  ctrl+d quit "
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

BODY_FRAME_THEN_EXIT = r'''
import sys
sys.stdout.write(FRAME); sys.stdout.flush()
'''

# Exits without writing anything. BODY_FRAME_THEN_EXIT cannot stand in for the
# "exited-before-ready" status: it flushes the required markers, so the harness
# records an observed frame before it notices the exit (readiness.py resolves
# ready_status from ready_ms first, then reaped).
BODY_NO_FRAME_THEN_EXIT = r'''
import sys
sys.exit(0)
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

BODY_FRAME_CANCEL_OUTCOME = r'''
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

BODY_FRAME_FOOTER_REDRAW = r'''
import sys, tty
sys.stdout.write(FRAME); sys.stdout.flush()
tty.setraw(0)
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b == b"\x04":
        break
    if b == b"\x03":
        sys.stdout.write(FRAME); sys.stdout.flush()
'''

BODY_FRAME_ECHO = r'''
import sys, tty
sys.stdout.write(FRAME); sys.stdout.flush()
tty.setraw(0)
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b in (b"\x04", b"\x03"):
        break
    sys.stdout.write("echo:" + b.decode("utf-8", "replace") + "\r\n")
    sys.stdout.flush()
'''

# Raw mode is entered *before* the frame is flushed. With the frame first and
# setraw second (BODY_FRAME_WAIT_QUIT), whether the child has reached setraw
# before the harness writes the probe key depends on scheduling: if the tty
# driver still has ECHO enabled it echoes the sentinel key straight back, and
# the harness legitimately reports observed-key-in-output. That made the probe
# status of a key-ignoring target a coin flip on a loaded machine.
BODY_FRAME_RAW_IGNORES_KEYS = r'''
import sys, tty
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


# ---------------------------------------------------------------------------
# Expected schema. These are the *contract*; every value below is asserted
# against live harness output by the tests that follow.
# ---------------------------------------------------------------------------

RUN_KEYS_BASE = {
    "run_index",
    "argv",
    "pid",
    "spawn",
    "ready",
    "input_probe",
    "idle",
    "cancel",
    "exit",
    "resources",
    "capture",
    "outcome",
}

SPAWN_KEYS = {"status", "error"}
READY_KEYS = {
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
READY_STATUSES = {"observed-frame-ready", "exited-before-ready", "ready-timeout"}
EXIT_KEYS = {
    "status",
    "exit_code",
    "signal",
    "elapsed_ms",
    "cleanup",
    "read_error",
}
EXIT_STATUSES = {"exited", "unreaped"}
CLEANUP_KEYS = {"method", "escalated", "reaped"}
# terminate_target() escalates quit keys -> SIGTERM -> SIGKILL, so this is the
# closed set of cleanup methods. Which one a run reaches depends on scheduling,
# so tests assert membership plus the escalated<->method invariant, never a
# specific method.
CLEANUP_METHODS = {"already-exited", "quit-keys", "sigterm", "sigkill"}
RESOURCE_KEYS = {"ru_maxrss", "ru_maxrss_unit", "method"}
CAPTURE_KEYS = {
    "retained_bytes",
    "total_bytes",
    "dropped_bytes",
    "max_capture_bytes",
    "alt_screen_seen",
}
IDLE_KEYS = {
    "status",
    "window_s",
    "exited_during_window",
    "cpu_seconds",
    "cpu_method",
    "idle_cpu_percent",
    "rss_samples",
    "rss_peak_sampled_bytes",
    "context_switches",
    "wakeups",
}
IDLE_STATUSES = {"observed", "short"}
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
CANCEL_STATUSES = {"observed", "unconfirmed", "skipped"}
EVIDENCE_SOURCES = {
    "default",
    "user-supplied",
    "user-supplied-regex",
    "user-supplied-mixed",
}
# The only latency claim the harness is ever allowed to make.
LATENCY_CLAIM_OBSERVATIONAL = "observational-marker-only"

PROBE_KEYS_MINIMAL = {"status", "ms", "bounded_ms", "char", "wrote_keys", "interpretation"}
PROBE_KEYS_WITH_DEFINITION = PROBE_KEYS_MINIMAL | {"definition"}
PROBE_STATUSES = {
    "observed-key-in-output",
    "exited-before-probe",
    "exited-before-key",
    "key-not-observed",
}

SUMMARY_BASE_KEYS = {
    "runs",
    "outcomes",
    "ready_ms",
    "cleanup_escalations",
    "scope_note",
}
SUMMARY_CANCEL_KEYS = {
    "observed",
    "observed_marker_ms",
    "observed_to_exit_ms",
    "skipped",
    "unconfirmed",
}
SUMMARY_IDLE_KEYS = {"run_index", "status", "idle_cpu_percent", "window_s"}


def cancel_block(**overrides):
    """A cancel block with every key at its 'not applicable' value."""
    block = {
        "status": None,
        "reason": None,
        "cancel_marker_observed_ms": None,
        "cancel_to_exit_ms": None,
        "wrote_keys": None,
        "evidence_source": None,
        "evidence_literals": [],
        "evidence_regexes": [],
        "missing_evidence": [],
        "exited_within_window": None,
        "interpretation": None,
        "latency_claim": None,
        "confound": None,
    }
    block.update(overrides)
    return block


def fake_run(index, ready_status="observed-frame-ready", ready_ms=1.0,
             outcome="ok", cancel=None, idle=None, escalated=False):
    """A minimal run dict shaped exactly like run_measurement() output."""
    return {
        "run_index": index,
        "argv": ["/bin/true"],
        "pid": 1000 + index,
        "spawn": {"status": "ok", "error": None},
        "ready": {
            "status": ready_status,
            "ms": ready_ms,
            "first_visible_ms": 0.5,
            "required": list(readiness.DEFAULT_REQUIRED),
            "missing": [],
            "header_seen": True,
            "alt_screen_seen": True,
            "definition": "spawn to ANSI-filtered PTY output containing all required markers",
            "note": "observed frame output, not proof that input handling is live",
        },
        "input_probe": None,
        "idle": idle,
        "cancel": cancel,
        "exit": {
            "status": "exited",
            "exit_code": 0,
            "signal": None,
            "elapsed_ms": 5.0,
            "cleanup": {"method": "quit-keys", "escalated": escalated, "reaped": True},
            "read_error": None,
        },
        "resources": {
            "ru_maxrss": 1024,
            "ru_maxrss_unit": readiness.rss_unit(),
            "method": "os.wait4 rusage of the directly waited child",
        },
        "capture": {
            "retained_bytes": 64,
            "total_bytes": 64,
            "dropped_bytes": 0,
            "max_capture_bytes": 2000000,
            "alt_screen_seen": True,
        },
        "outcome": outcome,
    }


class EvidenceSourceRuleTests(unittest.TestCase):
    """The evidence_source classification is pure config arithmetic; assert it.

    The harness derives evidence_source only from cfg.cancel_evidence and
    cfg.cancel_evidence_regex, so the rule is pinned here as a truth table
    rather than only through the slower PTY tests below.
    """

    def classify(self, literals, regexes):
        """Mirror of readiness.run_measurement's cancel evidence_source rule."""
        if regexes and not literals:
            return "user-supplied-regex"
        if regexes:
            return "user-supplied-mixed"
        if tuple(literals) != readiness.DEFAULT_CANCEL_EVIDENCE:
            return "user-supplied"
        return "default"

    def test_default_literals_are_default(self):
        self.assertEqual(self.classify(readiness.DEFAULT_CANCEL_EVIDENCE, ()), "default")

    def test_custom_literals_are_user_supplied(self):
        self.assertEqual(self.classify(("cancel",), ()), "user-supplied")

    def test_empty_literals_with_regex_is_regex(self):
        self.assertEqual(self.classify((), (r"outcome=cancelled",)), "user-supplied-regex")

    def test_literals_plus_regex_is_mixed(self):
        self.assertEqual(
            self.classify(("status: cancelled",), (r"outcome=cancelled",)),
            "user-supplied-mixed",
        )

    def test_every_classification_is_in_the_closed_set(self):
        cases = [
            (readiness.DEFAULT_CANCEL_EVIDENCE, ()),
            ((), ()),
            (("x",), ()),
            ((), ("p",)),
            (("x",), ("p",)),
        ]
        for literals, regexes in cases:
            with self.subTest(literals=literals, regexes=regexes):
                self.assertIn(self.classify(literals, regexes), EVIDENCE_SOURCES)


class EvidenceTrackerShapeTests(unittest.TestCase):
    """missing_evidence is the flat, string-typed alternative list."""

    def test_no_markers_is_incomplete_with_empty_missing(self):
        tracker = readiness.EvidenceTracker()
        tracker.feed(b"anything at all")
        self.assertFalse(tracker.complete)
        self.assertEqual(tracker.missing(), [])

    def test_missing_mixes_literals_and_regex_patterns_as_strings(self):
        tracker = readiness.EvidenceTracker(
            literals=("status: cancelled", "run cancelled"),
            regexes=(r"outcome\s*=\s*cancelled",),
        )
        tracker.feed(b"status: cancelled")
        self.assertTrue(tracker.complete)
        missing = tracker.missing()
        self.assertTrue(all(isinstance(item, str) for item in missing))
        self.assertEqual(sorted(missing), sorted(["run cancelled", r"outcome\s*=\s*cancelled"]))

    def test_duplicate_literal_collapses_to_one_entry(self):
        tracker = readiness.EvidenceTracker(literals=("x", "x"))
        self.assertEqual(tracker.missing(), ["x"])
        tracker.feed(b"x")
        self.assertTrue(tracker.complete)
        self.assertEqual(tracker.missing(), [])

    def test_any_of_semantics_not_all_of(self):
        tracker = readiness.EvidenceTracker(literals=("alpha", "beta"))
        tracker.feed(b"beta only")
        self.assertTrue(tracker.complete)
        self.assertEqual(tracker.missing(), ["alpha"])


class SummarizeRunsShapeTests(unittest.TestCase):
    """summarize_runs() key sets per mode, and its count arithmetic."""

    def test_ready_mode_summary_key_set(self):
        summary = readiness.summarize_runs(readiness.RunConfig(mode="ready"), [fake_run(0)])
        self.assertEqual(set(summary), SUMMARY_BASE_KEYS)
        self.assertNotIn("cancel", summary)
        self.assertNotIn("idle", summary)

    def test_idle_mode_summary_key_set(self):
        idle = {
            "status": "observed",
            "window_s": 1.0,
            "exited_during_window": False,
            "cpu_seconds": 0.5,
            "cpu_method": "test",
            "idle_cpu_percent": 50.0,
            "rss_samples": 2,
            "rss_peak_sampled_bytes": None,
            "context_switches": None,
            "wakeups": {"status": "unsupported", "reason": "x"},
        }
        summary = readiness.summarize_runs(
            readiness.RunConfig(mode="idle"), [fake_run(0, idle=idle)]
        )
        self.assertEqual(set(summary), SUMMARY_BASE_KEYS | {"idle"})
        self.assertEqual([entry["run_index"] for entry in summary["idle"]], [0])
        for entry in summary["idle"]:
            self.assertEqual(set(entry), SUMMARY_IDLE_KEYS)

    def test_cancel_mode_summary_key_set(self):
        runs = [
            fake_run(
                0,
                cancel=cancel_block(
                    status="observed",
                    cancel_marker_observed_ms=4.0,
                    cancel_to_exit_ms=6.0,
                    evidence_source="default",
                ),
            ),
            fake_run(
                1,
                cancel=cancel_block(status="unconfirmed", evidence_source="default"),
                escalated=True,
            ),
            fake_run(
                2,
                cancel=cancel_block(status="skipped", evidence_source="default"),
            ),
            # cancel=None stands for a run that never reached readiness.
            fake_run(3, ready_status="ready-timeout", ready_ms=None, outcome="ready-timeout"),
        ]
        summary = readiness.summarize_runs(readiness.RunConfig(mode="cancel"), runs)
        self.assertEqual(set(summary), SUMMARY_BASE_KEYS | {"cancel"})
        self.assertEqual(set(summary["cancel"]), SUMMARY_CANCEL_KEYS)
        self.assertEqual(summary["runs"], 4)
        self.assertEqual(summary["cleanup_escalations"], 1)
        self.assertEqual(summary["outcomes"], {"ok": 3, "ready-timeout": 1})
        cancel_summary = summary["cancel"]
        self.assertEqual(cancel_summary["observed"], 1)
        self.assertEqual(cancel_summary["unconfirmed"], 1)
        self.assertEqual(cancel_summary["skipped"], 1)
        self.assertEqual(cancel_summary["observed_marker_ms"], [4.0])
        self.assertEqual(cancel_summary["observed_to_exit_ms"], [6.0])

    def test_observed_latencies_excluded_when_unconfirmed(self):
        # An unconfirmed run must never contribute marker timings, even if a
        # caller-supplied block carries stray values.
        runs = [
            fake_run(
                0,
                cancel=cancel_block(
                    status="unconfirmed",
                    cancel_marker_observed_ms=99.0,
                    cancel_to_exit_ms=99.0,
                    evidence_source="default",
                ),
            ),
        ]
        summary = readiness.summarize_runs(readiness.RunConfig(mode="cancel"), runs)
        self.assertEqual(summary["cancel"]["observed_marker_ms"], [])
        self.assertEqual(summary["cancel"]["observed_to_exit_ms"], [])

    def test_ready_ms_aggregates_only_observed_ready_runs(self):
        runs = [
            fake_run(0, ready_ms=5.0),
            fake_run(1, ready_status="ready-timeout", ready_ms=None, outcome="ready-timeout"),
            fake_run(2, ready_status="exited-before-ready", ready_ms=None),
        ]
        summary = readiness.summarize_runs(readiness.RunConfig(mode="ready"), runs)
        self.assertEqual(summary["ready_ms"]["n"], 1)
        self.assertEqual(summary["ready_ms"]["max_ms"], 5.0)


@unittest.skipUnless(POSIX, "PTY harness requires POSIX fork/openpty/wait4")
class PtyShapeTestCase(unittest.TestCase):
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

    # -- ready / lifecycle block shapes -----------------------------------

    def ready_run(self, name, body, **overrides):
        overrides.setdefault("ready_timeout", 1.0)
        overrides.setdefault("timeout", 6.0)
        overrides.setdefault("quit_timeout", 0.5)
        overrides.setdefault("kill_grace", 0.5)
        return self.run_target(self.script(name, body), **overrides)

    def test_ready_key_set_is_identical_for_every_status(self):
        observed = self.ready_run("shape_ok.py", BODY_FRAME_WAIT_QUIT)
        timeout = self.ready_run(
            "shape_slow.py",
            r'''
import sys, time
sys.stdout.write("no frame yet\r\n"); sys.stdout.flush()
time.sleep(30)
''',
            ready_timeout=0.3,
        )
        exited = self.ready_run("shape_exit.py", BODY_NO_FRAME_THEN_EXIT)
        seen = set()
        for result in (observed, timeout, exited):
            ready = result["ready"]
            with self.subTest(status=ready["status"]):
                self.assertEqual(set(ready), READY_KEYS)
                seen.add(ready["status"])
                if ready["status"] == "observed-frame-ready":
                    self.assertIsNotNone(ready["ms"])
                    self.assertEqual(ready["missing"], [])
                else:
                    # A status that is not an observation carries no timing and
                    # reports every required marker as missing.
                    self.assertIsNone(ready["ms"])
                    self.assertEqual(ready["missing"], list(readiness.DEFAULT_REQUIRED))
            self.assertEqual(set(result["exit"]), EXIT_KEYS)
            self.assertEqual(set(result["exit"]["cleanup"]), CLEANUP_KEYS)
            self.assertEqual(set(result["resources"]), RESOURCE_KEYS)
            self.assertEqual(set(result["capture"]), CAPTURE_KEYS)
            self.assertEqual(set(result["spawn"]), SPAWN_KEYS)
            self.assertEqual(set(result), RUN_KEYS_BASE)
        self.assertEqual(seen, {"observed-frame-ready", "ready-timeout", "exited-before-ready"})

    def test_ready_missing_list_matches_required_markers_on_timeout(self):
        result = self.ready_run(
            "shape_missing.py",
            r'''
import sys, time
sys.stdout.write("starting up without a frame\r\n"); sys.stdout.flush()
time.sleep(30)
''',
            ready_timeout=0.3,
        )
        ready = result["ready"]
        self.assertEqual(ready["status"], "ready-timeout")
        self.assertEqual(ready["missing"], list(readiness.DEFAULT_REQUIRED))
        self.assertIsNone(ready["ms"])
        self.assertFalse(ready["header_seen"])
        self.assertIsNotNone(ready["first_visible_ms"])
        self.assertFalse(ready["alt_screen_seen"])

    def test_outcome_is_always_one_of_the_documented_values(self):
        allowed = set(readiness.FAILURE_OUTCOMES) | {"ok"}
        graceful = self.ready_run("shape_outcome.py", BODY_FRAME_WAIT_QUIT)
        self.assertIn(graceful["outcome"], allowed)
        self.assertEqual(graceful["outcome"], "ok")
        self.assertEqual(graceful["exit"]["status"], "exited")
        self.assertTrue(graceful["exit"]["cleanup"]["reaped"])
        self.assert_cleanup_contract(graceful["exit"]["cleanup"])

        # A run that never became ready is a documented failure outcome. Its
        # cleanup may escalate past the quit keys, which is exactly why the
        # graceful run above is checked with invariants and not a fixed method.
        timed_out = self.ready_run(
            "shape_outcome_timeout.py",
            r'''
import sys, time
sys.stdout.write("no frame yet\r\n"); sys.stdout.flush()
time.sleep(30)
''',
            ready_timeout=0.3,
        )
        self.assertIn(timed_out["outcome"], allowed)
        self.assertEqual(timed_out["outcome"], "ready-timeout")
        self.assertEqual(timed_out["exit"]["status"], "exited")
        self.assert_cleanup_contract(timed_out["exit"]["cleanup"])

    def assert_cleanup_contract(self, cleanup):
        """cleanup.method is in the closed set and escalation agrees with it."""
        self.assertIn(cleanup["method"], CLEANUP_METHODS)
        self.assertEqual(
            cleanup["escalated"], cleanup["method"] in {"sigterm", "sigkill"}
        )

    def test_spawn_error_result_omits_pid(self):
        # An empty argv is the deterministic pre-fork failure path: spawn_pty
        # raises ValueError before forking, so run_measurement reports
        # spawn-error without leaving a child behind. (A *nonexistent binary* is
        # not a spawn-error: spawn_pty forks, the child fails execvpe and exits
        # 127, and the parent reports spawn.status "ok" with outcome "ok" -- see
        # test_missing_binary_is_reported_as_exit_127.)
        result = readiness.run_measurement(readiness.RunConfig(timeout=1.0), [], 0)
        self.assertEqual(result["outcome"], "spawn-error")
        self.assertEqual(set(result["spawn"]), SPAWN_KEYS)
        self.assertEqual(result["spawn"]["status"], "error")
        self.assertIsNotNone(result["spawn"]["error"])
        self.assertNotIn("pid", result)
        # pid is the only run key that exists solely once a child was spawned.
        self.assertEqual(set(result), RUN_KEYS_BASE - {"pid"})
        self.assertIsNone(result["ready"])
        self.assertIsNone(result["exit"])
        self.assertIsNone(result["capture"])

    def test_missing_binary_is_reported_as_exit_127(self):
        # Known harness gap, pinned so it cannot regress silently: an argv[0]
        # that does not exist cannot produce spawn-error, because the exec
        # failure happens in the forked child (spawn_pty catches BaseException
        # and _exit(127)). The run is otherwise indistinguishable from a target
        # that started and exited cleanly.
        result = readiness.run_measurement(
            readiness.RunConfig(timeout=1.0), ["/nonexistent/nexus-shape-binary"], 0
        )
        self.assertEqual(result["spawn"]["status"], "ok")
        self.assertEqual(result["exit"]["exit_code"], 127)
        self.assertEqual(result["ready"]["status"], "exited-before-ready")
        self.assertEqual(result["outcome"], "ok")

    def test_idle_block_key_set(self):
        result = self.ready_run(
            "shape_idle.py",
            BODY_FRAME_WAIT_QUIT,
            mode="idle",
            idle_window=0.4,
            idle_interval=0.1,
        )
        idle = result["idle"]
        self.assertEqual(set(idle), IDLE_KEYS)
        self.assertIn(idle["status"], IDLE_STATUSES)
        self.assertEqual(set(idle["wakeups"]), {"status", "reason"})
        self.assertEqual(idle["wakeups"]["status"], "unsupported")
        self.assertIn(idle["context_switches"], (None, {"voluntary", "nonvoluntary"}))
        self.assertIsNone(result["cancel"])
        self.assertNotIn("idle", readiness.summarize_runs(readiness.RunConfig(mode="ready"), [result]))

    # -- cancel evidence block shapes -------------------------------------

    def cancel_run(self, name, body, **overrides):
        overrides.setdefault("mode", "cancel")
        overrides.setdefault("cancel_settle", 0.2)
        overrides.setdefault("cancel_timeout", 0.5)
        overrides.setdefault("timeout", 6.0)
        overrides.setdefault("quit_timeout", 0.5)
        overrides.setdefault("kill_grace", 0.5)
        return self.run_target(self.script(name, body), **overrides)

    def test_cancel_key_set_identical_for_all_three_statuses(self):
        cases = {
            "skipped": (
                "shape_skip.py",
                BODY_FRAME_THEN_EXIT,
                {"cancel_settle": 0.3},
            ),
            "unconfirmed": (
                "shape_unconf.py",
                BODY_FRAME_WAIT_QUIT,
                {"cancel_timeout": 0.3},
            ),
            "observed": (
                "shape_obs.py",
                BODY_FRAME_CANCEL_ACK,
                {"cancel_timeout": 1.0},
            ),
        }
        seen = set()
        for expected, (name, body, overrides) in cases.items():
            result = self.cancel_run(name, body, **overrides)
            cancel = result["cancel"]
            self.assertIsNotNone(cancel, expected)
            with self.subTest(status=expected):
                self.assertEqual(set(cancel), CANCEL_KEYS)
                self.assertEqual(cancel["status"], expected)
                self.assertIn(cancel["evidence_source"], EVIDENCE_SOURCES)
                # Non-observed statuses never claim a latency or a confound.
                if expected != "observed":
                    self.assertIsNone(cancel["latency_claim"])
                    self.assertIsNone(cancel["confound"])
                    self.assertIsNone(cancel["cancel_marker_observed_ms"])
                    self.assertIsNone(cancel["cancel_to_exit_ms"])
                else:
                    self.assertEqual(cancel["latency_claim"], LATENCY_CLAIM_OBSERVATIONAL)
                    self.assertIsNotNone(cancel["confound"])
            seen.add(cancel["status"])
        self.assertEqual(seen, CANCEL_STATUSES)

    def test_latency_claim_is_only_the_observational_sentinel(self):
        # Across every evidence configuration, the claim is either the single
        # observational sentinel or None. No authoritative/numeric claim exists.
        configs = [
            ({}, BODY_FRAME_CANCEL_ACK),
            ({"cancel_evidence": ("cancel",)}, BODY_FRAME_FOOTER_REDRAW),
            (
                {"cancel_evidence": (), "cancel_evidence_regex": (r"outcome=cancelled",)},
                BODY_FRAME_CANCEL_OUTCOME,
            ),
            (
                {
                    "cancel_evidence": ("status: cancelled",),
                    "cancel_evidence_regex": (r"outcome=cancelled",),
                },
                BODY_FRAME_CANCEL_ACK,
            ),
        ]
        observed_count = 0
        for index, (overrides, body) in enumerate(configs):
            result = self.cancel_run(f"shape_latency{index}.py", body, **overrides)
            cancel = result["cancel"]
            with self.subTest(config=overrides):
                self.assertIn(
                    cancel["latency_claim"], (None, LATENCY_CLAIM_OBSERVATIONAL)
                )
                if cancel["status"] == "observed":
                    observed_count += 1
                    self.assertEqual(cancel["latency_claim"], LATENCY_CLAIM_OBSERVATIONAL)
                    self.assertIn("not authoritative", cancel["interpretation"])
                    self.assertIn("cancel-induced", cancel["confound"])
                else:
                    self.assertIsNone(cancel["latency_claim"])
                    self.assertIsNone(cancel["interpretation"])
                    self.assertIsNotNone(cancel["reason"])
        self.assertEqual(observed_count, 4)

    def test_evidence_source_matrix_end_to_end(self):
        literal = {"cancel_evidence": ("cancel",)}
        regex_only = {"cancel_evidence": (), "cancel_evidence_regex": (r"outcome=cancelled",)}
        mixed = {
            "cancel_evidence": ("status: cancelled",),
            "cancel_evidence_regex": (r"outcome=cancelled",),
        }
        configs = [
            ({}, "default", BODY_FRAME_CANCEL_ACK),
            (literal, "user-supplied", BODY_FRAME_FOOTER_REDRAW),
            (regex_only, "user-supplied-regex", BODY_FRAME_CANCEL_OUTCOME),
            (mixed, "user-supplied-mixed", BODY_FRAME_CANCEL_ACK),
        ]
        self.assertEqual(
            {expected for _, expected, _ in configs}, EVIDENCE_SOURCES
        )
        for index, (overrides, expected, body) in enumerate(configs):
            result = self.cancel_run(f"shape_source{index}.py", body, **overrides)
            cancel = result["cancel"]
            with self.subTest(expected=expected):
                self.assertEqual(cancel["status"], "observed")
                self.assertEqual(cancel["evidence_source"], expected)

    def test_evidence_literals_and_regexes_echo_the_configuration(self):
        result = self.cancel_run(
            "shape_echo.py",
            BODY_FRAME_CANCEL_OUTCOME,
            cancel_evidence=("status: cancelled", "run cancelled"),
            cancel_evidence_regex=(r"outcome=cancelled", r"aborted"),
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["evidence_source"], "user-supplied-mixed")
        self.assertEqual(
            cancel["evidence_literals"], ["status: cancelled", "run cancelled"]
        )
        self.assertEqual(
            cancel["evidence_regexes"], [r"outcome=cancelled", r"aborted"]
        )
        self.assertEqual(set(cancel), CANCEL_KEYS)

    def test_default_literals_echo_default_configuration(self):
        result = self.cancel_run("shape_default_echo.py", BODY_FRAME_CANCEL_ACK)
        cancel = result["cancel"]
        self.assertEqual(cancel["evidence_source"], "default")
        self.assertEqual(cancel["evidence_literals"], list(readiness.DEFAULT_CANCEL_EVIDENCE))
        self.assertEqual(cancel["evidence_regexes"], [])

    def test_missing_evidence_lists_unseen_alternatives(self):
        # Mixed config where only the regex matched: the unobserved literal is
        # still reported, and the matched alternative is not.
        result = self.cancel_run(
            "shape_missing_mixed.py",
            BODY_FRAME_CANCEL_OUTCOME,
            cancel_evidence=("status: cancelled", "run cancelled"),
            cancel_evidence_regex=(r"outcome=cancelled",),
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "observed")
        self.assertEqual(cancel["missing_evidence"], ["status: cancelled", "run cancelled"])

    def test_missing_evidence_complete_for_default_observation(self):
        # Default cancel evidence is an any-of set, so an observation is
        # complete when *one* alternative matched: missing_evidence lists the
        # alternatives still unseen, it is not a completeness flag. "Status:
        # Cancelled" matches the second default literal case-insensitively,
        # which leaves the first one reported as missing.
        result = self.cancel_run("shape_missing_default.py", BODY_FRAME_CANCEL_ACK)
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "observed")
        self.assertEqual(cancel["evidence_literals"], list(readiness.DEFAULT_CANCEL_EVIDENCE))
        self.assertEqual(cancel["missing_evidence"], ["run cancelled"])
        self.assertNotIn("status: cancelled", cancel["missing_evidence"])
        self.assertIsNone(cancel["reason"])

    def test_missing_evidence_lists_all_defaults_when_unconfirmed(self):
        result = self.cancel_run(
            "shape_missing_unconf.py", BODY_FRAME_FOOTER_REDRAW, cancel_timeout=0.3
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "unconfirmed")
        self.assertEqual(cancel["missing_evidence"], list(readiness.DEFAULT_CANCEL_EVIDENCE))
        self.assertFalse(cancel["exited_within_window"])
        self.assertTrue(cancel["wrote_keys"])
        self.assertIn("still alive", cancel["reason"])

    def test_missing_evidence_is_empty_for_skipped(self):
        result = self.cancel_run(
            "shape_missing_skip.py", BODY_FRAME_THEN_EXIT, cancel_settle=0.3
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "skipped")
        self.assertEqual(cancel["missing_evidence"], [])
        self.assertIsNone(cancel["wrote_keys"])
        self.assertTrue(cancel["exited_within_window"])
        self.assertIsNone(cancel["interpretation"])
        self.assertIsNone(cancel["confound"])
        self.assertIn("finished", cancel["reason"])

    def test_empty_evidence_configuration_is_unconfirmed_not_observed(self):
        # Nothing to match: the run must not claim an observation.
        result = self.cancel_run(
            "shape_no_evidence.py",
            BODY_FRAME_CANCEL_ACK,
            cancel_evidence=(),
            cancel_evidence_regex=(),
            cancel_timeout=0.5,
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "unconfirmed")
        self.assertEqual(cancel["evidence_source"], "user-supplied")
        self.assertEqual(cancel["missing_evidence"], [])
        self.assertEqual(cancel["evidence_literals"], [])
        self.assertIsNone(cancel["latency_claim"])

    def test_observed_cancel_reports_exit_timing_when_target_exits(self):
        result = self.cancel_run(
            "shape_timing.py", BODY_FRAME_CANCEL_ACK, cancel_timeout=2.0
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "observed")
        self.assertTrue(cancel["exited_within_window"])
        self.assertIsNotNone(cancel["cancel_marker_observed_ms"])
        self.assertGreaterEqual(cancel["cancel_marker_observed_ms"], 0.0)
        self.assertIsNotNone(cancel["cancel_to_exit_ms"])
        self.assertGreaterEqual(cancel["cancel_to_exit_ms"], cancel["cancel_marker_observed_ms"])

    def test_cancel_block_absent_without_readiness(self):
        result = self.cancel_run(
            "shape_not_ready.py",
            r'''
import sys, time
sys.stdout.write("no frame\r\n"); sys.stdout.flush()
time.sleep(30)
''',
            ready_timeout=0.3,
        )
        self.assertEqual(result["ready"]["status"], "ready-timeout")
        self.assertIsNone(result["cancel"])
        self.assertIsNone(result["idle"])
        summary = readiness.summarize_runs(readiness.RunConfig(mode="cancel"), [result])
        self.assertEqual(summary["cancel"]["observed"], 0)
        self.assertEqual(summary["cancel"]["unconfirmed"], 0)
        self.assertEqual(summary["cancel"]["skipped"], 0)

    def test_generic_footer_text_never_produces_default_evidence(self):
        # The footer is redrawn verbatim after Ctrl+C; no evidence is claimed.
        result = self.cancel_run(
            "shape_footer.py", BODY_FRAME_FOOTER_REDRAW, cancel_timeout=0.5
        )
        cancel = result["cancel"]
        self.assertEqual(cancel["status"], "unconfirmed")
        self.assertIsNone(cancel["cancel_marker_observed_ms"])
        self.assertEqual(cancel["evidence_source"], "default")
        self.assertEqual(len(cancel["missing_evidence"]), len(readiness.DEFAULT_CANCEL_EVIDENCE))

    # -- input probe block shapes -----------------------------------------

    def test_input_probe_key_sets_per_status(self):
        observed = self.ready_run(
            "shape_probe_obs.py",
            BODY_FRAME_ECHO,
            probe_input=True,
            probe_char="z",
            probe_timeout=2.0,
        )
        not_observed = self.ready_run(
            "shape_probe_no.py",
            BODY_FRAME_RAW_IGNORES_KEYS,
            probe_input=True,
            probe_char="z",
            probe_timeout=0.3,
        )
        exited = self.ready_run(
            "shape_probe_exit.py",
            BODY_FRAME_THEN_EXIT,
            probe_input=True,
            probe_char="z",
        )
        blocks = [observed["input_probe"], not_observed["input_probe"], exited["input_probe"]]
        statuses = {block["status"] for block in blocks}
        self.assertTrue(statuses <= PROBE_STATUSES, statuses)
        self.assertIn("observed-key-in-output", statuses)
        for block in blocks:
            with self.subTest(status=block["status"]):
                if block["status"] == "observed-key-in-output":
                    self.assertEqual(set(block), PROBE_KEYS_WITH_DEFINITION)
                    self.assertIsNotNone(block["ms"])
                else:
                    # Documented asymmetry: only the observed branch carries a
                    # "definition" string. Consumers must tolerate its absence.
                    self.assertEqual(set(block), PROBE_KEYS_MINIMAL)
                    self.assertIsNone(block["ms"])
                self.assertEqual(block["char"], "z")
                self.assertIsInstance(block["bounded_ms"], float)
                self.assertGreater(block["bounded_ms"], 0.0)
                self.assertIn("observational", block["interpretation"])

    def test_input_probe_absent_without_flag(self):
        result = self.ready_run("shape_probe_absent.py", BODY_FRAME_WAIT_QUIT)
        self.assertIsNone(result["input_probe"])

    def test_input_probe_only_observed_branch_is_keyed_differently(self):
        # Pin the asymmetry so a future change to one branch fails loudly.
        # "z" is not a substring of FRAME, so an observation can only come from
        # the echoed probe key, never from leftover frame text.
        observed = self.ready_run(
            "shape_probe_def.py",
            BODY_FRAME_ECHO,
            probe_input=True,
            probe_char="z",
            probe_timeout=2.0,
        )["input_probe"]
        unobserved = self.ready_run(
            "shape_probe_nodef.py",
            BODY_FRAME_RAW_IGNORES_KEYS,
            probe_input=True,
            probe_char="z",
            probe_timeout=0.3,
        )["input_probe"]
        self.assertEqual(observed["status"], "observed-key-in-output")
        self.assertNotEqual(unobserved["status"], "observed-key-in-output")
        self.assertEqual(set(observed) - set(unobserved), {"definition"})
        self.assertEqual(set(unobserved) - set(observed), set())


if __name__ == "__main__":
    unittest.main()