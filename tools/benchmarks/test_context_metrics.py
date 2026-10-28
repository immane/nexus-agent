"""Offline tests for context benchmark event capture and metric definitions."""

import json
import io
from http.client import IncompleteRead
from types import SimpleNamespace
import unittest
import tempfile
from pathlib import Path
from unittest.mock import patch
from urllib.error import URLError

from context_metrics import derive_metrics, load_events, main as metrics_main
from run_nexus_swe_task import (
    capture_event,
    capture_terminal_patch,
    complete_run_usage,
    event_record,
    event_stream_response,
    main as runner_main,
    new_run_observation,
    observe_run_event,
    stream_end_error,
    summarize_usage_by_turn,
    tool_wait_report,
)


def event(seq, kind, detail):
    return {"seq": seq, "kind": kind, "detail": detail}


class HeartbeatTests(unittest.TestCase):
    def run_stream(self, schedule, max_seconds=86400):
        clock = [0.0]

        class Stream:
            def __iter__(self):
                for at, value in schedule:
                    clock[0] = at
                    yield value if isinstance(value, bytes) else b"data: " + json.dumps(value).encode() + b"\n"

            def close(self):
                pass

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            task = root / "task.json"
            task.write_text(json.dumps({"instance_id": "task-1", "base_commit": "base", "problem_statement": "Fix it"}))
            output = root / "output"
            argv = ["runner", "--task-json", str(task), "--workspace", str(root),
                    "--output-dir", str(output), "--provider", "fake", "--model", "fake",
                    "--max-run-seconds", str(max_seconds)]
            with (
                patch("sys.argv", argv),
                patch.dict("os.environ", {"NEXUS_BENCH_SMOKE_TOKEN": "test-only"}),
                patch("sys.stdout", new_callable=io.StringIO),
                patch("sys.stderr", new_callable=io.StringIO) as stderr,
                patch("run_nexus_swe_task.time.monotonic", side_effect=lambda: clock[0]),
                patch("run_nexus_swe_task.validate_container_git_checkout"),
                patch("run_nexus_swe_task.git", side_effect=[SimpleNamespace(stdout="base\n"), SimpleNamespace(stdout="")]),
                patch("run_nexus_swe_task.request", side_effect=[{"session": "s1"}, {"run": "r1"}, {}]) as request_mock,
                patch("run_nexus_swe_task.urlopen", return_value=Stream()),
                patch("run_nexus_swe_task.collect_patch", return_value="diff"),
            ):
                runner_main()
            run_dir = next((output / "runs").iterdir())
            return (stderr.getvalue(), load_events(run_dir / "events.jsonl"),
                    json.loads((run_dir / "result.json").read_text()), request_mock.call_args_list)

    def test_wait_reports_are_rate_limited_text_free_and_not_sequenced_events(self):
        schedule = [
            (0, event(0, "run-started", {})),
            (1, event(1, "tool-started", {"call": "c1", "tool": "host_exec", "args_preview": "private args"})),
            (30, b": ping\n"), (31, b": ping\n"), (60, b": ping\n"), (121, b": ping\n"),
            (122, event(2, "tool-output", {"call": "c1", "preview": "private output", "truncated": False})),
            (152, b": ping\n"),
            (153, event(3, "tool-finished", {"call": "c1", "outcome": {"status": "Succeeded"}})),
            (182, b": ping\n"),
            (183, {**event(4, "run-finished", {"outcome": "completed"}), "terminal": True}),
        ]
        log, events, result, requests = self.run_stream(schedule)
        reports = [json.loads(line.removeprefix("[heartbeat] ")) for line in log.splitlines() if line.startswith("[heartbeat] ")]
        self.assertEqual([r["elapsed_seconds"] for r in reports], [30, 60, 121, 152, 182])
        self.assertTrue(reports[2]["active_tools"][0]["quiet"])
        self.assertEqual(reports[3]["active_tools"][0]["no_output_seconds"], 30)
        self.assertFalse(reports[3]["active_tools"][0]["quiet"])
        self.assertEqual(reports[4]["active_tools"], [])
        self.assertIn('"preview_bytes": 14', log)
        self.assertIn("command progress unknown", log)
        self.assertNotIn("private output", log)
        self.assertNotIn("private args", log)
        self.assertEqual([e["seq"] for e in events], list(range(5)))
        self.assertEqual(result["event_sequence_gaps"], 0)
        self.assertEqual(len(requests), 2, "quiet tools must not be cancelled automatically")

    def test_wall_clock_budget_is_checked_on_heartbeat_and_cancelled_once(self):
        log, events, result, requests = self.run_stream([
            (0, event(0, "run-started", {})),
            (1, event(1, "tool-started", {"call": "c1", "tool": "host_exec"})),
            (30, b": ping\n"), (31, b": ping\n"),
            (32, {**event(2, "run-finished", {"outcome": "cancelled"}), "terminal": True}),
        ], max_seconds=10)
        self.assertEqual(result["budget_stop_reason"], "wall-clock budget reached")
        self.assertEqual(len(requests), 3)
        self.assertEqual(requests[-1].args[2], "/sessions/s1/runs/r1/cancel")
        self.assertEqual(len(events), 3)
        self.assertIn("requesting run cancellation", log)

    def test_wait_report_tracks_each_call_independently(self):
        reports = tool_wait_report({"c1": 0, "c2": 100}, {"c1": 110}, 130)
        self.assertEqual([r["no_output_seconds"] for r in reports], [20, 30])
        self.assertEqual([r["elapsed_seconds"] for r in reports], [130, 30])

    def test_recovered_tool_timeout_does_not_end_or_cancel_benchmark_run(self):
        log, events, result, requests = self.run_stream([
            (0, event(0, "run-started", {})),
            (1, event(1, "tool-started", {"call": "c1", "tool": "host_exec"})),
            (3, event(2, "tool-finished", {"call": "c1", "outcome": {
                "status": "TimedOut", "effect": "Unknown", "evidence": "HostObserved",
                "content": "command timed out; process cleanup completed",
            }})),
            (4, event(3, "model-request-started", {"turn": "t2", "purpose": "task"})),
            (5, event(4, "tool-started", {"call": "c2", "tool": "host_read"})),
            (6, event(5, "tool-finished", {"call": "c2", "outcome": {"status": "Succeeded"}})),
            (7, {**event(6, "run-finished", {"outcome": "completed"}), "terminal": True}),
        ])
        self.assertEqual(result["outcome"], "completed")
        self.assertEqual(result["tool_calls"], 2)
        self.assertIsNone(result["budget_stop_reason"])
        self.assertTrue(result["terminal_observed"])
        self.assertEqual(result["event_sequence_gaps"], 0)
        self.assertEqual(events[2]["detail"]["outcome"]["status"], "TimedOut")
        self.assertEqual(len(requests), 2, "a tool timeout is not a run cancellation")
        self.assertIn("status=TimedOut", log)

    def test_unconfirmed_cleanup_denies_tools_but_allows_model_completion(self):
        log, events, result, requests = self.run_stream([
            (0, event(0, "run-started", {})),
            (1, event(1, "tool-started", {"call": "c1", "tool": "host_exec"})),
            (3, event(2, "tool-finished", {"call": "c1", "outcome": {
                "status": "TimedOut", "effect": "Unknown", "evidence": "Uncertain",
            }})),
            (4, event(3, "model-request-started", {"turn": "t2", "purpose": "task"})),
            (5, event(4, "tool-finished", {"call": "c2", "outcome": {
                "status": "Denied", "effect": "NotStarted", "evidence": "HostObserved",
                "content": "tool dispatch blocked while a worker remains quarantined",
            }})),
            (6, event(5, "model-request-started", {"turn": "t3", "purpose": "task"})),
            (7, event(6, "assistant-text", {"text": "Completed with uncertain tool effects."})),
            (8, {**event(7, "run-finished", {"outcome": "completed"}), "terminal": True}),
        ])
        self.assertEqual(result["outcome"], "completed")
        self.assertIsNone(result["error"])
        self.assertIsNone(result["budget_stop_reason"])
        self.assertTrue(result["terminal_observed"])
        self.assertEqual(result["event_sequence_gaps"], 0)
        self.assertEqual(events[2]["detail"]["outcome"], {
            "status": "TimedOut", "effect": "Unknown", "evidence": "Uncertain",
            "output_bytes": 0,
        })
        self.assertEqual(events[4]["detail"]["outcome"]["status"], "Denied")
        self.assertEqual(events[4]["detail"]["outcome"]["effect"], "NotStarted")
        self.assertEqual(len(requests), 2, "unconfirmed cleanup is not a run cancellation")
        self.assertNotIn("requesting run cancellation", log)


class EventCaptureTests(unittest.TestCase):
    def test_incomplete_http_read_finalizes_runner_and_requests_cancellation(self):
        class InterruptedStream:
            closed = False

            def __iter__(self):
                yield b'data: {"seq":0,"kind":"run-started","detail":{}}\n'
                yield b'data: {"seq":1,"kind":"tool-finished","detail":{"outcome":{"status":"Succeeded"}}}\n'
                raise IncompleteRead(b"partial", 10)

            def close(self):
                self.closed = True

        stream = InterruptedStream()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            task = root / "task.json"
            task.write_text(json.dumps({
                "instance_id": "task-1", "base_commit": "base", "problem_statement": "Fix it",
            }))
            output = root / "output"
            argv = [
                "runner", "--task-json", str(task), "--workspace", str(root),
                "--output-dir", str(output), "--provider", "fake", "--model", "fake",
            ]
            with (
                patch("sys.argv", argv),
                patch.dict("os.environ", {"NEXUS_BENCH_SMOKE_TOKEN": "test-only"}),
                patch("sys.stdout", new_callable=io.StringIO),
                patch("sys.stderr", new_callable=io.StringIO),
                patch("run_nexus_swe_task.validate_container_git_checkout"),
                patch("run_nexus_swe_task.git", side_effect=[
                    SimpleNamespace(stdout="base\n"), SimpleNamespace(stdout=""),
                ]),
                patch("run_nexus_swe_task.request", side_effect=[
                    {"session": "s1"}, {"run": "r1"}, {},
                ]) as request_mock,
                patch("run_nexus_swe_task.urlopen", return_value=stream),
                patch("run_nexus_swe_task.collect_patch") as collect,
            ):
                self.assertEqual(runner_main(), 1)
            self.assertTrue(stream.closed)
            collect.assert_not_called()
            self.assertEqual(request_mock.call_count, 3)
            self.assertEqual(request_mock.call_args.args[2], "/sessions/s1/runs/r1/cancel")
            run_dir = next((output / "runs").iterdir())
            result = json.loads((run_dir / "result.json").read_text())
            self.assertFalse(result["terminal_observed"])
            self.assertIsNone(result["outcome"])
            self.assertEqual(result["event_stream_error"], "event stream interrupted")
            self.assertEqual(result["cancel_after_stream_loss"], "requested")
            self.assertEqual(json.loads((run_dir / "manifest.json").read_text())["status"], "incomplete")
            self.assertFalse((run_dir / "prediction.jsonl").exists())
            self.assertFalse((output / "task-1.prediction.jsonl").exists())
            self.assertEqual(
                json.loads((run_dir / "metrics.json").read_text()),
                derive_metrics(
                    load_events(run_dir / "events.jsonl"),
                    json.loads((run_dir / "manifest.json").read_text()), result,
                ),
            )

    def test_write_failure_disables_future_capture_without_raising(self):
        class FailingStream:
            def write(self, _value):
                raise OSError("disk full")

            def close(self):
                pass

        stream, error = capture_event(FailingStream(), event(1, "run-started", {}), 0.1)
        self.assertIsNone(stream)
        self.assertEqual(error, "event log write failed")
        self.assertEqual(capture_event(stream, event(2, "run-finished", {}), 0.2), (None, None))

    def test_event_records_drop_text_and_tool_bodies_without_mutating_input(self):
        tool = event(1, "tool-finished", {"outcome": {"content": "secret output", "truncated": False}})
        record = event_record(tool, 0.25)
        self.assertNotIn("secret output", str(record))
        self.assertEqual(record["detail"]["outcome"]["output_bytes"], len("secret output"))
        self.assertEqual(tool["detail"]["outcome"]["content"], "secret output")
        text = event_record(event(2, "assistant-text", {"text": "private answer"}), 0.5)
        self.assertNotIn("private answer", str(text))
        self.assertEqual(text["detail"]["text_bytes"], len("private answer"))
        approval = event_record(
            event(3, "approval-required", {
                "approval": "a1", "call": "c1", "args_preview": "/private/path",
                "scope": "/private", "session_directory": "/private/home",
            }),
            0.75,
        )
        self.assertEqual(approval["detail"], {"approval": "a1", "call": "c1"})

    def test_incomplete_final_line_can_be_excluded_only_when_explicit(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "events.jsonl"
            path.write_text('{"schema_version":1,"seq":0,"kind":"run-started","detail":{}}\n{"schema_version":')
            with self.assertRaises(ValueError):
                load_events(path)
            self.assertEqual(len(load_events(path, allow_incomplete_tail=True)), 1)

    def test_interrupted_sse_stream_is_reported_and_suppressed_for_finalization(self):
        class InterruptedStream:
            def __iter__(self):
                yield b"data: {}\n\n"
                raise OSError("connection reset")

            def close(self):
                pass

        status = {"error": None}
        records = []
        with patch("run_nexus_swe_task.urlopen", return_value=InterruptedStream()):
            with event_stream_response("http://local/events", {}, status) as stream:
                for record in stream:
                    records.append(record)
        self.assertEqual(records, [b"data: {}\n\n"])
        self.assertEqual(status["error"], "event stream interrupted")

        status = {"error": None}
        with patch("run_nexus_swe_task.urlopen", side_effect=URLError("offline")):
            with event_stream_response("http://local/events", {}, status) as stream:
                self.assertEqual(list(stream), [])
        self.assertEqual(status["error"], "event stream connection failed")

    def test_tool_finish_then_stream_loss_is_incomplete_and_never_captures_patch(self):
        class InterruptedStream:
            def __iter__(self):
                yield b'data: {"seq":1,"kind":"tool-finished","detail":{"outcome":{"status":"Succeeded"}}}\n\n'
                raise OSError("connection reset")

            def close(self):
                pass

        observation = new_run_observation()
        status = {"error": None}
        with patch("run_nexus_swe_task.urlopen", return_value=InterruptedStream()):
            with event_stream_response("http://local/events", {}, status) as stream:
                for raw in stream:
                    observe_run_event(observation, json.loads(raw[6:]))

        self.assertEqual(observation["last_tool_outcome"]["status"], "Succeeded")
        self.assertIsNone(observation["outcome"])
        self.assertEqual(stream_end_error(observation, status["error"]), "event stream interrupted")
        with patch("run_nexus_swe_task.collect_patch") as collect:
            self.assertIsNone(capture_terminal_patch(Path("/workspace"), "base", observation))
            collect.assert_not_called()

    def test_terminal_event_allows_patch_capture_after_prior_tool_result(self):
        observation = new_run_observation()
        observe_run_event(observation, event(1, "tool-finished", {"outcome": {"status": "Succeeded"}}))
        observe_run_event(observation, {
            "kind": "run-finished",
            "terminal": True,
            "detail": {"outcome": "completed", "error": None},
        })
        with patch("run_nexus_swe_task.collect_patch", return_value="patch") as collect:
            self.assertEqual(capture_terminal_patch(Path("/workspace"), "base", observation), "patch")
            collect.assert_called_once_with(Path("/workspace"), "base")


class UsageAccountingTests(unittest.TestCase):
    def test_final_usage_updates_replace_prior_counters_per_turn(self):
        usage, complete = summarize_usage_by_turn(
            {
                "turn-1": {"input": 90, "output": 20},
                "turn-2": {"input": 10, "output": 4},
            },
            {"turn-1", "turn-2"},
        )
        self.assertEqual(usage, {"input": 100, "output": 24})
        self.assertTrue(complete)
        latest, complete = summarize_usage_by_turn(
            {"turn-1": {"input": 120, "output": 22}}, {"turn-1"}
        )
        self.assertEqual(latest, {"input": 120, "output": 22})
        self.assertTrue(complete)
        missing, complete = summarize_usage_by_turn(
            {"turn-1": {"input": None, "output": 22}}, {"turn-1"}
        )
        self.assertEqual(missing, {"input": 0, "output": 22})
        self.assertFalse(complete)

    def test_reported_run_usage_requires_terminal_and_all_request_turns(self):
        counters = {
            "turn-1": {"input": 90, "output": 20},
            "turn-2": {"input": 10, "output": 4},
        }
        usage, complete = complete_run_usage(counters, set(counters), 2, True)
        self.assertEqual(usage, {"input": 100, "output": 24})
        self.assertTrue(complete)
        self.assertEqual(complete_run_usage(counters, set(counters), 2, False), (None, False))
        self.assertEqual(complete_run_usage(counters, {"turn-1"}, 2, True), (None, False))


class MetricsTests(unittest.TestCase):
    def test_offline_cli_preserves_collection_errors_and_incomplete_log_tail(self):
        events = [
            event_record(event(0, "run-started", {}), 0),
            event_record(event(1, "run-finished", {"outcome": "completed"}), 1),
        ]
        result = {
            "outcome": "completed", "budget_stop_reason": None,
            "terminal_observed": True, "event_sequence_gaps": 2,
            "event_stream_error": "event stream close failed",
            "telemetry_error": "event log write failed",
        }
        expected = derive_metrics(events, {}, result)
        self.assertFalse(expected["completeness"]["event_stream_complete"])
        self.assertEqual(expected["completeness"]["received_event_sequence_gaps"], 2)
        self.assertEqual(expected["completeness"]["event_sequence_gaps"], 0)
        self.assertTrue(expected["completeness"]["terminal_observed_by_runner"])
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "events.jsonl").write_text(
                "".join(json.dumps(record) + "\n" for record in events) + '{"schema_version":'
            )
            (root / "manifest.json").write_text("{}")
            (root / "result.json").write_text(json.dumps(result))
            argv = [
                "context_metrics", "--events", str(root / "events.jsonl"),
                "--manifest", str(root / "manifest.json"),
                "--result", str(root / "result.json"), "--output", str(root / "metrics.json"),
            ]
            with patch("sys.argv", argv), patch("sys.stdout", new_callable=io.StringIO):
                metrics_main()
            self.assertEqual(json.loads((root / "metrics.json").read_text()), expected)

    def setUp(self):
        self.manifest = {"context_window": {"requested_tokens": 65_536, "mode": "runtime-budget-only"}}
        self.result = {
            "outcome": "completed",
            "budget_stop_reason": None,
            "tool_calls": 2,
            "duration_seconds": 12.0,
        }

    def test_derives_invocation_usage_compaction_and_recovery(self):
        events = [
            event(0, "run-started", {}),
            event(1, "model-request-started", {"turn": "t1", "purpose": "summary", "estimated_input_tokens": 900, "context_window_tokens": 65_536, "context_trigger_percent": 65, "context_target_percent": 10, "compaction_strategy": "summarize", "projected_tool_outputs": 0}),
            event(2, "usage", {"finality": "Final", "input": 700, "output": 100}),
            event(3, "model-request-finished", {"turn": "t1", "response_validated": True}),
            event(4, "context-reduced", {"method": "summarize"}),
            event(5, "model-request-started", {"turn": "t2", "purpose": "task", "estimated_input_tokens": 500, "context_trigger_percent": 65, "context_target_percent": 10, "compaction_strategy": "summarize", "projected_tool_outputs": 2}),
            event(6, "usage", {"finality": "Final", "input": 450, "output": 20}),
            event(7, "model-request-finished", {"turn": "t2", "response_validated": True}),
            event(8, "run-finished", {"outcome": "completed"}),
        ]
        metrics = derive_metrics(events, self.manifest, self.result, {"evaluation_outcome": "resolved"})
        self.assertTrue(metrics["completeness"]["input_usage_complete"])
        self.assertEqual(metrics["tokens"]["input_total"], 1150)
        self.assertEqual(metrics["context"]["provider_reported_peak_input_tokens_observed"], 700)
        self.assertEqual(metrics["context"]["runtime_estimated_peak_input_tokens"], 900)
        self.assertEqual(metrics["context"]["runtime_configured_or_discovered_windows_observed"], [65_536])
        self.assertEqual(metrics["context"]["runtime_compaction_policies_observed"], [
            {"trigger_percent": 65, "target_percent": 10, "strategy": "summarize"}
        ])
        self.assertEqual(metrics["compaction"]["committed_by_method"], {"summarize": 1})
        self.assertEqual(metrics["compaction"]["summary_requests_started"], 1)
        self.assertEqual(metrics["compaction"]["summary_responses_validated"], 1)
        self.assertEqual(metrics["compaction"]["summary_commits"], 1)
        self.assertEqual(metrics["compaction"]["post_compaction_recovery"][0]["response_validated"], True)
        self.assertEqual(metrics["compaction"]["projected_tool_result_occurrences"], 2)
        self.assertTrue(metrics["normal_completion"])
        self.assertTrue(metrics["evaluation"]["resolved"])

    def test_summary_access_retry_preserves_each_invocation_usage(self):
        events = [
            event(0, "run-started", {}),
            event(1, "model-request-started", {"turn": "t1", "purpose": "summary"}),
            event(2, "model-request-finished", {"turn": "t1", "response_validated": True}),
            event(3, "usage", {"finality": "Final", "input": 100, "output": 20}),
            event(4, "model-request-started", {"turn": "t2", "purpose": "summary"}),
            event(5, "model-request-finished", {"turn": "t2", "response_validated": True}),
            event(6, "usage", {"finality": "Final", "input": 200, "output": 30}),
            event(7, "run-finished", {"outcome": "completed"}),
        ]
        metrics = derive_metrics(events, self.manifest, self.result)
        self.assertEqual(metrics["completeness"]["provider_invocations_observed"], 2)
        self.assertEqual(metrics["compaction"]["summary_requests_started"], 2)
        self.assertEqual(metrics["compaction"]["summary_responses_validated"], 2)
        self.assertEqual(metrics["tokens"]["input_total"], 300)
        self.assertEqual(metrics["tokens"]["output_total"], 50)
        self.assertTrue(metrics["completeness"]["input_usage_complete"])

    def test_missing_usage_and_sequence_gaps_remain_explicit(self):
        events = [
            event(1, "model-request-started", {"turn": "t1", "purpose": "task", "estimated_input_tokens": 123}),
            event(3, "usage", {"finality": "Final", "input": None, "output": 4}),
            event(4, "run-finished", {"outcome": "completed"}),
        ]
        metrics = derive_metrics(events, self.manifest, self.result)
        self.assertFalse(metrics["completeness"]["input_usage_complete"])
        self.assertFalse(metrics["completeness"]["event_stream_complete"])
        self.assertFalse(metrics["compaction"]["reduction_observation_complete"])
        self.assertIsNone(metrics["tokens"]["input_total"])
        self.assertEqual(metrics["tokens"]["input_reported_sum"], 0)
        self.assertEqual(metrics["completeness"]["event_sequence_gaps"], 2)
        self.assertIsNone(metrics["time_to_first_valid_patch_seconds"])
        self.assertIsNone(metrics["repeated_tool_call_candidates"])

    def test_absent_terminal_and_provider_usage_do_not_become_success_or_zero(self):
        events = [event(1, "model-request-started", {"turn": "t1", "purpose": "task"})]
        metrics = derive_metrics(events, self.manifest, self.result)
        self.assertIsNone(metrics["normal_completion"])
        self.assertIsNone(metrics["tokens"]["input_total"])
        self.assertTrue(metrics["context"]["provider_reported_peak_is_lower_bound"])

    def test_final_usage_updates_for_one_turn_are_reconciled_not_summed(self):
        events = [
            event(0, "run-started", {}),
            event(1, "model-request-started", {"turn": "t1", "purpose": "task"}),
            event(2, "usage", {"finality": "Final", "input": 90, "output": 20}),
            event(3, "usage", {"finality": "Final", "input": 120, "output": 22}),
            event(4, "model-request-finished", {"turn": "t1", "response_validated": True}),
            event(5, "run-finished", {"outcome": "completed"}),
        ]
        result = dict(self.result)
        metrics = derive_metrics(events, self.manifest, result)
        self.assertEqual(metrics["tokens"]["input_total"], 120)
        self.assertEqual(metrics["tokens"]["output_total"], 22)
        self.assertEqual(metrics["tokens"]["input_observations"], 1)
        self.assertEqual(metrics["completeness"]["final_usage_events_observed"], 2)

    def test_collection_errors_are_derived_without_mutating_run_outcome(self):
        events = [
            event(0, "run-started", {}),
            event(1, "run-finished", {"outcome": "completed"}),
        ]
        metrics = derive_metrics(
            events,
            self.manifest,
            self.result,
            collection={
                "terminal_observed": True,
                "event_stream_error": "event stream interrupted",
                "telemetry_error": "event log write failed",
                "received_event_sequence_gaps": 2,
            },
        )
        self.assertFalse(metrics["completeness"]["event_stream_complete"])
        self.assertEqual(metrics["completeness"]["event_sequence_gaps"], 0)
        self.assertEqual(metrics["completeness"]["received_event_sequence_gaps"], 2)
        self.assertEqual(metrics["completeness"]["event_stream_error"], "event stream interrupted")
        self.assertEqual(metrics["completeness"]["telemetry_error"], "event log write failed")
        self.assertTrue(metrics["normal_completion"], "observed terminal outcome remains distinct from telemetry coverage")


if __name__ == "__main__":
    unittest.main()
