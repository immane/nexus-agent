"""Versioned, offline derivation of context-run metrics from caller JSONL."""

import argparse
import json
from pathlib import Path

SCHEMA_VERSION = 1
METRICS_VERSION = 1


def load_events(path, allow_incomplete_tail=False):
    events = []
    with Path(path).open(encoding="utf-8") as source:
        for line_number, line in enumerate(source, 1):
            if not line.strip():
                continue
            try:
                event = json.loads(line)
            except json.JSONDecodeError as error:
                if allow_incomplete_tail and not line.endswith("\n"):
                    break
                raise ValueError(f"invalid event JSON at line {line_number}: {error}") from None
            if event.get("schema_version") != SCHEMA_VERSION:
                raise ValueError(f"unsupported event schema at line {line_number}")
            events.append(event)
    return events


def derive_metrics(events, manifest, result, evaluation=None, collection=None):
    """Derive only observations supported by the captured events; missing stays unknown."""
    # Use persisted runner facts for both online generation and offline replay.
    # Explicit collection facts may supplement older result artifacts.
    collection = {
        "telemetry_error": result.get("telemetry_error"),
        "event_stream_error": result.get("event_stream_error"),
        "received_event_sequence_gaps": result.get("event_sequence_gaps"),
        **({"terminal_observed": result["terminal_observed"]} if "terminal_observed" in result else {}),
        **(collection or {}),
    }
    requests_by_purpose = {"task": 0, "summary": 0, "unknown": 0}
    validated_by_purpose = {"task": 0, "summary": 0, "unknown": 0}
    current_request = None
    request_turns = set()
    final_usage_records = {}
    final_usage_events = 0
    estimate_peak = None
    estimate_peaks_by_purpose = {}
    runtime_windows = set()
    runtime_policies = set()
    compactions = {}
    projected_outputs = 0
    recoveries = []
    active_recoveries = []
    run_started = False
    terminal_seen = False
    control_delivery_uncertain = False
    sequence_gaps = 0
    last_seq = None

    for event in events:
        seq = event.get("seq")
        if isinstance(seq, int):
            if last_seq is None and seq != 0:
                sequence_gaps += max(1, seq)
            elif last_seq is not None and seq != last_seq + 1:
                sequence_gaps += max(1, seq - last_seq - 1)
            last_seq = seq
        kind = event.get("kind")
        detail = event.get("detail") or {}
        if kind == "run-started":
            run_started = True
        elif kind == "context-reduced":
            method = detail.get("method", "unknown")
            compactions[method] = compactions.get(method, 0) + 1
            recovery = {
                "method": method,
                "next_request_turn": None,
                "response_validated": None,
            }
            recoveries.append(recovery)
            active_recoveries.append(recovery)
        elif kind == "model-request-started":
            current_request = {
                "turn": detail.get("turn"),
                "purpose": detail.get("purpose", "unknown"),
            }
            requests_by_purpose.setdefault(current_request["purpose"], 0)
            requests_by_purpose[current_request["purpose"]] += 1
            turn = current_request["turn"]
            if turn is not None:
                request_turns.add(turn)
            estimate = detail.get("estimated_input_tokens")
            if isinstance(estimate, int):
                estimate_peak = estimate if estimate_peak is None else max(estimate_peak, estimate)
                estimate_peaks_by_purpose[current_request["purpose"]] = max(
                    estimate_peaks_by_purpose.get(current_request["purpose"], estimate),
                    estimate,
                )
            if isinstance(detail.get("context_window_tokens"), int):
                runtime_windows.add(detail["context_window_tokens"])
            runtime_policies.add((
                detail.get("context_trigger_percent"),
                detail.get("context_target_percent"),
                detail.get("compaction_strategy"),
            ))
            projected_outputs += detail.get("projected_tool_outputs", 0) or 0
            for recovery in active_recoveries[:]:
                if recovery["next_request_turn"] is None:
                    recovery["next_request_turn"] = turn
        elif kind == "model-request-finished":
            turn = detail.get("turn")
            response_validated = detail.get("response_validated")
            if current_request is not None and current_request["turn"] == turn:
                if response_validated is True:
                    validated_by_purpose.setdefault(current_request["purpose"], 0)
                    validated_by_purpose[current_request["purpose"]] += 1
            for recovery in active_recoveries[:]:
                if recovery["next_request_turn"] == turn:
                    if response_validated is True or response_validated is False:
                        recovery["response_validated"] = response_validated
                    active_recoveries.remove(recovery)
        elif kind == "usage" and detail.get("finality") == "Final":
            final_usage_events += 1
            turn = current_request.get("turn") if current_request else None
            purpose = current_request.get("purpose", "unknown") if current_request else "unknown"
            key = turn if turn is not None else f"unknown-seq-{seq}"
            final_usage_records[key] = {
                "turn": turn,
                "purpose": purpose,
                "input": detail.get("input"),
                "output": detail.get("output"),
            }
        elif kind == "run-finished":
            terminal_seen = True
            error = detail.get("error")
            if isinstance(error, dict) and error.get("category") == "ResourceLimit":
                control_delivery_uncertain = True

    invocation_count = sum(requests_by_purpose.values())
    usage_by_purpose = {"task": [], "summary": [], "unknown": []}
    input_sum = output_sum = input_known = output_known = 0
    input_peak = output_peak = None
    missing_input = missing_output = False
    for record in final_usage_records.values():
        usage_by_purpose.setdefault(record["purpose"], []).append(
            {key: record[key] for key in ("turn", "input", "output")}
        )
        for field, value in (("input", record["input"]), ("output", record["output"])):
            if not isinstance(value, int) or value < 0:
                if field == "input":
                    missing_input = True
                else:
                    missing_output = True
                continue
            if field == "input":
                input_sum += value
                input_known += 1
                input_peak = value if input_peak is None else max(input_peak, value)
            else:
                output_sum += value
                output_known += 1
                output_peak = value if output_peak is None else max(output_peak, value)
    final_usage_turns = {record["turn"] for record in final_usage_records.values() if record["turn"] is not None}
    telemetry_error = collection.get("telemetry_error")
    event_stream_error = collection.get("event_stream_error")
    collection_terminal = collection.get("terminal_observed", terminal_seen)
    event_stream_complete = (
        run_started
        and terminal_seen
        and sequence_gaps == 0
        and event_stream_error is None
        and telemetry_error is None
    )
    invocation_usage_complete = (
        event_stream_complete
        and len(request_turns) == invocation_count
        and len(final_usage_turns) == invocation_count
        and len(final_usage_records) == invocation_count
        and invocation_count > 0
    )
    input_complete = invocation_usage_complete and not missing_input
    output_complete = invocation_usage_complete and not missing_output
    outcome = result.get("outcome")
    budget_stop = result.get("budget_stop_reason")
    if collection_terminal and outcome is not None and "budget_stop_reason" in result:
        normal_completion = outcome == "completed" and budget_stop is None
    else:
        normal_completion = None
    evaluation_outcome = "not-run"
    if evaluation:
        evaluation_outcome = evaluation.get("evaluation_outcome", "unknown")
    usage_peaks_by_purpose = {
        purpose: max(
            (record["input"] for record in records if isinstance(record["input"], int)),
            default=None,
        )
        for purpose, records in usage_by_purpose.items()
    }

    window = manifest.get("context_window", {})
    return {
        "metrics_version": METRICS_VERSION,
        "completeness": {
            "terminal_event_observed": terminal_seen,
            "run_started_event_observed": run_started,
            "event_sequence_gaps": sequence_gaps,
            "received_event_sequence_gaps": collection.get("received_event_sequence_gaps"),
            "event_stream_complete": event_stream_complete,
            "event_stream_error": event_stream_error,
            "telemetry_error": telemetry_error,
            "terminal_observed_by_runner": collection_terminal,
            "input_usage_complete": input_complete,
            "output_usage_complete": output_complete,
            "provider_invocations_observed": invocation_count,
            "final_usage_turns_observed": len(final_usage_turns),
            "final_usage_events_observed": final_usage_events,
        },
        "evaluation": {
            "outcome": evaluation_outcome,
            "resolved": (
                True if evaluation_outcome == "resolved"
                else False if evaluation_outcome in {"unresolved", "empty-patch"}
                else None
            ) if evaluation else None,
        },
        "compaction": {
            "committed_reductions": sum(compactions.values()),
            "committed_by_method": compactions,
            "reduction_observation_complete": event_stream_complete and not control_delivery_uncertain,
            "reduction_observation_note": (
                "Control-channel resource exhaustion makes committed-reduction coverage uncertain."
                if control_delivery_uncertain else None
            ),
            "attempt_count": None,
            "attempt_count_note": "Only committed reductions are currently observable.",
            "summary_requests_started": requests_by_purpose.get("summary", 0),
            "summary_responses_validated": validated_by_purpose.get("summary", 0),
            "summary_commits": compactions.get("summarize", 0),
            "projected_tool_result_occurrences": projected_outputs,
            "post_compaction_recovery": recoveries,
            "recovery_validated_response_count": sum(r["response_validated"] is True for r in recoveries),
            "recovery_invalid_response_count": sum(r["response_validated"] is False for r in recoveries),
            "recovery_unobserved_count": sum(r["response_validated"] is None for r in recoveries),
        },
        "context": {
            "requested_window_tokens": window.get("requested_tokens"),
            "window_mode": window.get("mode", "unknown"),
            "runtime_configured_or_discovered_windows_observed": sorted(runtime_windows),
            "runtime_compaction_policies_observed": [
                {"trigger_percent": trigger, "target_percent": target, "strategy": strategy}
                for trigger, target, strategy in sorted(
                    runtime_policies,
                    key=lambda policy: tuple(str(value) for value in policy),
                )
            ],
            "provider_reported_peak_input_tokens_observed": input_peak,
            "provider_reported_peak_is_lower_bound": not input_complete,
            "provider_reported_peak_input_tokens_by_purpose": usage_peaks_by_purpose,
            "runtime_estimated_peak_input_tokens": estimate_peak,
            "runtime_estimated_peak_input_tokens_by_purpose": estimate_peaks_by_purpose,
        },
        "tokens": {
            "input_total": input_sum if input_complete else None,
            "input_reported_sum": input_sum,
            "input_observations": input_known,
            "output_total": output_sum if output_complete else None,
            "output_reported_sum": output_sum,
            "output_observations": output_known,
            "usage_by_purpose": usage_by_purpose,
        },
        "normal_completion": normal_completion,
        "tool_calls": result.get("tool_calls"),
        "duration_seconds": result.get("duration_seconds"),
        "time_to_first_nonempty_patch_seconds": result.get("time_to_first_nonempty_patch_seconds"),
        "time_to_first_valid_patch_seconds": None,
        "repeated_tool_call_candidates": None,
        "limitations": [
            "Recovery currently means a validated provider response after a committed reduction, not proven task progress.",
            "First-valid-patch evaluation and repeated-tool-call review are not implemented.",
        ],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--events", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--result", type=Path, required=True)
    parser.add_argument("--evaluation", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    manifest = json.loads(args.manifest.read_text())
    result = json.loads(args.result.read_text())
    events = load_events(args.events, allow_incomplete_tail=result.get("telemetry_error") is not None)
    evaluation = json.loads(args.evaluation.read_text()) if args.evaluation else None
    metrics = derive_metrics(events, manifest, result, evaluation)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(metrics, indent=2) + "\n")
    print(json.dumps(metrics, indent=2))


if __name__ == "__main__":
    main()
