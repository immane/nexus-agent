#!/usr/bin/env python3
"""Sequential, explicit SWE-bench diagnostic batches using the existing launcher."""

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import uuid

import swebench
import evaluate_swe_suite
from prepare_swe_suite import build_source_sha256, load_suite
from run_nexus_swe_task import write_json

HERE = Path(__file__).resolve().parent


def read_suite(path, selected=()):
    suite = load_suite(path)
    tasks = suite["tasks"]
    ids = [task["instance_id"] for task in tasks]
    if set(selected) - set(ids):
        raise ValueError("selected task is not in the suite")
    return suite, [task for task in tasks if not selected or task["instance_id"] in selected]


def save_batch(path, batch):
    temporary = path.with_suffix(".tmp")
    write_json(temporary, batch)
    temporary.replace(path)


def run_command(argv, log):
    """Stream diagnostics, preserving the launcher's bounded cancellation/cleanup."""
    child, pump, errors = swebench.logged_process(argv, os.environ.copy(), log)
    try:
        return child.wait()
    finally:
        if child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=45)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=10)
                errors.append("launcher killed; inspect ownership state before another attempt")
        pump.join(timeout=5)
        if errors or pump.is_alive():
            raise RuntimeError("incomplete batch child capture/cleanup; inspect " + str(log))


def collect_attempt(root, instance):
    launches = list(root.glob("run-*/launch.json"))
    if not launches:
        return {"classification": "preparation-failed", "agent_outcome": None,
                "official_resolved": None, "launch": None}
    if len(launches) != 1:
        raise ValueError("ambiguous launch artifacts; refusing to select a run")
    launch_path = launches[0]
    launch = json.loads(launch_path.read_text())
    if launch.get("task") != instance:
        raise ValueError("launch task does not match batch entry")
    result_path = launch_path.parent / "artifacts" / f"{instance}.result.json"
    result = json.loads(result_path.read_text()) if result_path.exists() else None
    if result is not None and result.get("instance_id") != instance:
        raise ValueError("runner result identity mismatch")
    if launch.get("cleanup") != "complete":
        classification = "cleanup-required"
    elif result is None:
        classification = "prepared" if launch.get("status") == "prepared" else "preparation-failed"
    elif result.get("terminal_observed") is not True:
        classification = "incomplete-run"
    elif result.get("outcome") != "completed":
        classification = "agent-failed"
    elif not result.get("patch_bytes"):
        classification = "empty-patch"
    else:
        classification = "completed-unscored"
    metric_paths = list((launch_path.parent / "artifacts/runs").glob("*/metrics.json"))
    if len(metric_paths) > 1:
        raise ValueError("ambiguous runner metrics")
    metrics = json.loads(metric_paths[0].read_text()) if metric_paths else None
    return {"classification": classification, "launch": str(launch_path),
            "agent_outcome": result.get("outcome") if result else None,
            "official_resolved": None,
            "duration_seconds": result.get("duration_seconds") if result else None,
            "tool_calls": result.get("tool_calls") if result else None,
            "usage": result.get("usage") if result else None,
            "budget_stop_reason": result.get("budget_stop_reason") if result else None,
            "terminal_observed": result.get("terminal_observed") if result else None,
            "patch_bytes": result.get("patch_bytes") if result else None,
            "telemetry_error": result.get("telemetry_error") if result else None,
            "event_stream_error": result.get("event_stream_error") if result else None,
            "event_sequence_gaps": result.get("event_sequence_gaps") if result else None,
            "metrics_path": str(metric_paths[0]) if metric_paths else None,
            "telemetry_completeness": metrics.get("completeness") if metrics else None,
            "compaction": metrics.get("compaction") if metrics else None}


def summarize(batch):
    counts = {}
    for entry in batch["tasks"]:
        state = entry.get("classification", "unstarted")
        counts[state] = counts.get(state, 0) + 1
    return {"suite": batch["suite"], "status": batch["status"],
            "action": batch.get("action"), "evaluation_mode": batch.get("mode"),
            "selected": len(batch["tasks"]), "classifications": counts,
            "note": "Completion is not resolution. Official scoring is separate; unknown is not success.",
            "tasks": batch["tasks"]}


def evaluate_batch(args, suite, tasks):
    source = None
    if args.mode == "prediction":
        if args.batch is None:
            raise ValueError("prediction evaluation requires --batch from a live suite run")
        source = json.loads(args.batch.read_text())
        if (source["action"] != "run" or source["suite_sha256"] !=
                hashlib.sha256(args.suite.read_bytes()).hexdigest()):
            raise ValueError("prediction batch must match this suite and contain live attempts")
        if len({entry["instance_id"] for entry in source["tasks"]}) != len(source["tasks"]):
            raise ValueError("duplicate task identity in source batch")
    elif args.batch is not None:
        raise ValueError("--batch is only used for prediction evaluation or summarization")
    directory = args.work_root / "suites" / ("evaluation-" + args.mode + "-" + uuid.uuid4().hex)
    directory.mkdir(mode=0o700, parents=True)
    path = directory / "batch.json"
    batch = {"schema_version": 1, "suite": suite["name"], "action": "evaluate", "mode": args.mode,
             "suite_sha256": hashlib.sha256(args.suite.read_bytes()).hexdigest(),
             "source_batch": str(args.batch.resolve()) if source else None,
             "source_batch_sha256": hashlib.sha256(args.batch.read_bytes()).hexdigest() if source else None,
             "status": "running", "tasks": [{**task, "classification": "unstarted"} for task in tasks]}
    save_batch(path, batch)
    print(f"Evaluation batch: {path}", flush=True)
    codes = []
    try:
        for entry in batch["tasks"]:
            instance = entry["instance_id"]
            prediction = None
            if source:
                original = next((item for item in source["tasks"] if item["instance_id"] == instance), None)
                if original is None or not original.get("launch"):
                    entry["classification"] = "no-prediction"
                    codes.append(1)
                    save_batch(path, batch)
                    continue
                prediction = Path(original["launch"]).parent / "artifacts" / f"{instance}.prediction.jsonl"
                if not prediction.is_file():
                    entry["classification"] = "no-prediction"
                    codes.append(1)
                    save_batch(path, batch)
                    continue
                entry["agent_attempt"] = original
            entry["classification"] = "evaluating"
            save_batch(path, batch)
            options = argparse.Namespace(work_root=args.work_root, suite=args.suite,
                                         task=instance, mode=args.mode, prediction=prediction,
                                         output_root=directory / "official")
            evaluation_dir, code = evaluate_swe_suite.evaluate(options)
            evidence_path = evaluation_dir / "evaluation.json"
            evidence = json.loads(evidence_path.read_text())
            entry.update(evaluation=str(evidence_path), exit_code=code,
                         classification=evidence["status"],
                         official_outcome=evidence.get("evaluation_outcome"),
                         official_resolved=(evidence.get("evaluation_outcome") == "resolved")
                         if evidence["status"] == "completed" else None,
                         expectation_met=evidence.get("expectation_met"))
            codes.append(code)
            save_batch(path, batch)
            print(f"{instance}: {entry['classification']} / {entry['official_outcome']}", flush=True)
            if evidence["status"] in ("timeout", "interrupted", "error"):
                raise RuntimeError("evaluation stopped; inspect failed evaluator and container state")
        batch["status"] = "finished"
    except BaseException:
        batch["status"] = "interrupted-or-failed"
        raise
    finally:
        save_batch(path, batch)
        write_json(directory / "summary.json", summarize(batch))
    return 0 if all(code == 0 for code in codes) else 1


def execute_batch(args, suite, tasks):
    profiles_path = (args.profiles or args.work_root / "suite-profiles.json").resolve()
    profiles = json.loads(profiles_path.read_text())
    available = swebench.available_tasks(args.work_root, profiles)
    preparation = json.loads((args.work_root / "suite-preparation.json").read_text())
    if preparation["suite_sha256"] != hashlib.sha256(args.suite.read_bytes()).hexdigest():
        raise ValueError("suite changed after preparation")
    source_hash = build_source_sha256()
    # Check the whole selection before building or making any model request.
    for task in tasks:
        instance = task["instance_id"]
        if instance not in available or not available[instance]["image"]:
            raise ValueError(f"task metadata/adapter missing: {instance}; run prepare_swe_suite.py --images")
        swebench.agent_task(available[instance]["row"])
        prepared = preparation["tasks"][instance]
        source = args.work_root / f"{instance}.task.json"
        if hashlib.sha256(source.read_bytes()).hexdigest() != prepared["task_sha256"]:
            raise ValueError("prepared task metadata changed: " + instance)
        if swebench.inspect_image(available[instance]["image"])["Id"] != prepared["images"]["derived"]["id"]:
            raise ValueError("prepared adapter image changed: " + instance)
        if prepared["images"].get("build_source_sha256") != source_hash:
            raise ValueError("adapter worker sources changed; explicitly rebuild images: " + instance)
    defaults = suite["defaults"]
    limits = {key: getattr(args, key) if getattr(args, key) is not None else defaults[key]
              for key in ("max_run_seconds", "max_tool_calls", "tool_log_bytes")}
    if min(limits["max_run_seconds"], limits["max_tool_calls"]) <= 0 or not 0 <= limits["tool_log_bytes"] <= 65536:
        raise ValueError("invalid batch budgets")
    batch_root = args.work_root / "suites" / (suite["name"] + "-" +
                 datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + uuid.uuid4().hex[:8])
    batch_root.mkdir(mode=0o700, parents=True)
    batch_path = batch_root / "batch.json"
    batch = {"schema_version": 1, "suite": suite["name"], "action": args.action,
             "suite_sha256": hashlib.sha256(args.suite.read_bytes()).hexdigest(),
             "profile_sha256": hashlib.sha256(profiles_path.read_bytes()).hexdigest(),
             "limits": limits, "provider_override": args.provider, "model_override": args.model,
             "hosted_inference_allowed": bool(getattr(args, "allow_hosted_inference", False)),
             "status": "running", "tasks": [
                 {**task, "classification": "unstarted"} for task in tasks]}
    save_batch(batch_path, batch)
    print(f"Batch: {batch_path}", flush=True)
    try:
        if args.build:
            swebench.command(["cargo", "build", "--release", "-p", "nexus-server", "--features",
                              "container", "--locked", "--offline"], cwd=swebench.REPO, timeout=600)
        for entry in batch["tasks"]:
            instance = entry["instance_id"]
            attempt = batch_root / instance
            attempt.mkdir(mode=0o700)
            # The parent metadata retains evaluator fields; each attempt only
            # needs agent-safe metadata. Gold is never copied into task mounts.
            write_json(attempt / f"{instance}.task.json", swebench.agent_task(available[instance]["row"]))
            entry["classification"] = "running"
            entry["attempt_root"] = str(attempt)
            save_batch(batch_path, batch)
            argv = [sys.executable, "-u", str(HERE / "swebench.py"), "run", instance,
                    "--work-root", str(attempt), "--profiles", str(profiles_path),
                    "--config", str(args.config.resolve()), "--no-build", "--verify-task-environment"]
            for name, value in limits.items():
                argv += ["--" + name.replace("_", "-"), str(value)]
            for name in ("provider", "model", "model_output_tokens"):
                if getattr(args, name) is not None:
                    argv += ["--" + name.replace("_", "-"), str(getattr(args, name))]
            if getattr(args, "allow_hosted_inference", False):
                argv.append("--allow-hosted-inference")
            if args.action == "prepare":
                argv.append("--prepare-only")
            entry["argv"] = argv
            code = run_command(argv, attempt / "launcher.log")
            entry.update(collect_attempt(attempt, instance))
            entry["exit_code"] = code
            if code and entry["classification"] in ("prepared", "completed-unscored"):
                entry["classification"] = "launcher-failed"
            save_batch(batch_path, batch)
            if code == 130 or entry["classification"] == "cleanup-required":
                raise RuntimeError("batch stopped after interruption or uncertain cleanup")
        batch["status"] = "finished"
    except BaseException:
        batch["status"] = "interrupted-or-failed"
        # Refresh the active entry after the child has attempted cleanup.
        for entry in batch["tasks"]:
            if entry["classification"] == "running":
                entry.update(collect_attempt(Path(entry["attempt_root"]), entry["instance_id"]))
        raise
    finally:
        save_batch(batch_path, batch)
        write_json(batch_root / "summary.json", summarize(batch))
    print(json.dumps(summarize(batch), indent=2))
    success = "prepared" if args.action == "prepare" else "completed-unscored"
    return 0 if all(entry["classification"] == success and entry["exit_code"] == 0
                    for entry in batch["tasks"]) else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("list", "prepare", "run", "evaluate", "summarize"))
    parser.add_argument("--suite", type=Path, default=HERE / "swe_suite.json")
    parser.add_argument("--work-root", type=Path,
                        default=swebench.REPO.parent / "nexus-benchmark-work/lite-diagnostics-v1")
    parser.add_argument("--task", action="append", default=[])
    parser.add_argument("--profiles", type=Path)
    parser.add_argument("--config", type=Path, default=Path.home() / ".config/nexus-agent/config.json")
    parser.add_argument("--provider")
    parser.add_argument("--model")
    parser.add_argument("--model-output-tokens", type=int)
    parser.add_argument("--allow-hosted-inference", action="store_true",
                        help="explicitly allow a public model endpoint; inference may incur uncapped provider costs")
    for name in ("max-run-seconds", "max-tool-calls", "tool-log-bytes"):
        parser.add_argument("--" + name, type=int)
    parser.add_argument("--build", action=argparse.BooleanOptionalAction, default=True)
    parser.add_argument("--batch", type=Path, help="batch.json to summarize without external operations")
    parser.add_argument("--mode", choices=("gold", "empty", "prediction"), default="gold",
                        help="explicit official evaluation mode; defaults to oracle preflight")
    args = parser.parse_args()
    if args.allow_hosted_inference and args.action != "run":
        parser.error("--allow-hosted-inference is only valid for run")
    if args.action == "summarize":
        if args.batch is None:
            parser.error("summarize requires --batch")
        print(json.dumps(summarize(json.loads(args.batch.read_text())), indent=2))
        return 0
    suite, tasks = read_suite(args.suite, args.task)
    if args.action == "list":
        for task in tasks:
            print(f"{task['instance_id']}\t{task['domain']}\t{task['purpose']}")
        return 0
    args.work_root = args.work_root.resolve()
    if args.action == "evaluate":
        return evaluate_batch(args, suite, tasks)
    return execute_batch(args, suite, tasks)


if __name__ == "__main__":
    def interrupted(_signum, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print("batch interrupted; no automatic retry", file=sys.stderr)
        raise SystemExit(130)
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"diagnostic suite: {error}", file=sys.stderr)
        raise SystemExit(2)
