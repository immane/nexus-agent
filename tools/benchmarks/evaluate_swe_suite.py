#!/usr/bin/env python3
"""Evaluate one canonical task with pinned SWE-bench; never invoke a model.

Invocation authorizes uv installation and CPU Docker evaluation. Canonical rows
must include the 5.0.2 image, eval_script, log_parser and eval_type fields.
"""

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import uuid

from swebench import inspect_image

HERE = Path(__file__).resolve().parent
VERSION = "5.0.2"
TEST_TIMEOUT = 1800
PROCESS_TIMEOUT = 3600
UV = ["uv", "run", "--isolated", "--no-project", "--with", f"swebench=={VERSION}", "python"]


def write_json(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load_task(suite, root, instance_id):
    if not re.fullmatch(r"[A-Za-z0-9_.-]+__[A-Za-z0-9_.-]+-\d+", instance_id):
        raise ValueError("invalid task instance ID")
    if suite.get("evaluator_version") != VERSION or suite.get("platform") != "linux/amd64":
        raise ValueError("suite must pin evaluator_version 5.0.2 and platform linux/amd64")
    selected = [task for task in suite["tasks"] if task["instance_id"] == instance_id]
    if len(selected) != 1:
        raise ValueError("task must appear exactly once in suite")
    row = json.loads((root / f"{instance_id}.task.json").read_text())
    required = ("instance_id", "repo", "base_commit", "version", "problem_statement",
                "patch", "test_patch", "FAIL_TO_PASS", "PASS_TO_PASS", "image",
                "eval_script", "log_parser", "eval_type")
    if not isinstance(row, dict) or any(key not in row for key in required):
        raise ValueError("full canonical SWE-bench 5.0.2 row required (not agent-safe metadata)")
    if row["instance_id"] != instance_id:
        raise ValueError("canonical row instance ID mismatch")
    if not isinstance(row["image"], str) or not re.fullmatch(
        r"swebench/sweb\.eval\.x86_64\.[A-Za-z0-9_.-]+(?::[A-Za-z0-9_.-]+)?", row["image"]
    ):
        raise ValueError("canonical row must name an official swebench x86_64 instance image")
    if not isinstance(row["patch"], str) or not row["patch"].strip():
        raise ValueError("canonical gold patch is missing or empty")
    return row, selected[0]


def prediction_patch(path, instance_id):
    """Accept a saved single-task prediction object, JSON array, or JSONL."""
    text = path.read_text()
    if path.suffix == ".jsonl":
        rows = [json.loads(line) for line in text.splitlines() if line.strip()]
    else:
        rows = json.loads(text)
        if isinstance(rows, dict):
            rows = [rows]
    if not isinstance(rows, list) or len(rows) != 1 or not isinstance(rows[0], dict):
        raise ValueError("prediction must contain exactly one task")
    row = rows[0]
    if row.get("instance_id") != instance_id or not isinstance(row.get("model_patch"), str):
        raise ValueError("prediction task mismatch or missing string model_patch")
    return row["model_patch"]


def verify_prepared_inputs(root, suite_path, row):
    preparation = json.loads((root / "suite-preparation.json").read_text())
    if preparation["suite_sha256"] != sha256(suite_path):
        raise ValueError("suite changed after preparation")
    prepared = preparation["tasks"][row["instance_id"]]
    if prepared["task_sha256"] != sha256(root / f"{row['instance_id']}.task.json"):
        raise ValueError("canonical metadata changed after pinned preparation")
    base = prepared["images"]["base"]
    actual = inspect_image(row["image"])
    if actual["Id"] != base["id"] or actual["Architecture"] != "amd64" or actual["Os"] != "linux":
        raise ValueError("official scoring image differs from prepared identity")
    repository = row["image"].rsplit(":", 1)[0]
    digest = next((value for value in base["repo_digests"]
                   if value.startswith(repository + "@sha256:") and value in actual["RepoDigests"]), None)
    if digest is None:
        raise ValueError("no verified official repository digest for scoring")
    return {"id": actual["Id"], "digest": digest,
            "preparation_sha256": sha256(root / "suite-preparation.json")}


def classify_report(report, instance_id):
    """Validate the one-task denominator and outcome before trusting a report."""
    fields = ("submitted", "completed", "resolved", "unresolved", "empty_patch", "error",
              "incomplete", "infra_failure", "ambiguous_failure")
    ids = {}
    for field in fields:
        value = report.get(field + "_ids")
        if not isinstance(value, list) or any(item != instance_id for item in value) or len(value) > 1:
            raise ValueError(f"invalid official report membership: {field}")
        ids[field] = set(value)
        count_key = field + "_instances"
        if field != "incomplete" and (type(report.get(count_key)) is not int or report[count_key] != len(value)):
            raise ValueError(f"invalid official report denominator: {count_key}")
    if type(report.get("total_instances")) is not int or report["total_instances"] != 1 or ids["submitted"] != {instance_id}:
        raise ValueError("official report does not cover exactly the submitted task")
    outcomes = ("resolved", "unresolved", "empty_patch", "error", "incomplete")
    if sum(bool(ids[field]) for field in outcomes) != 1:
        raise ValueError("missing or conflicting official outcomes")
    if not (ids["resolved"] | ids["unresolved"]).issubset(ids["completed"]):
        raise ValueError("resolved/unresolved task was not completed")
    if ids["completed"] & (ids["empty_patch"] | ids["incomplete"]):
        raise ValueError("skipped task marked completed")
    if not (ids["infra_failure"] | ids["ambiguous_failure"]).issubset(ids["unresolved"] | ids["error"]):
        raise ValueError("failure annotations contradict official outcome")
    for field in ("infra_failure", "ambiguous_failure", *outcomes):
        if ids[field]:
            return field.replace("_", "-")


def run_command(argv, cwd, log_path):
    """Bound the process lifetime; preserve logs and stop its group on interruption."""
    env = dict(os.environ, DOCKER_DEFAULT_PLATFORM="linux/amd64")
    with log_path.open("wb") as log:
        child = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                 stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            return child.wait(timeout=PROCESS_TIMEOUT)
        except BaseException:
            # Killing the host harness does not guarantee Docker container cleanup.
            try:
                os.killpg(child.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                child.wait()
            raise


def evaluate(args):
    root, suite_path = args.work_root.resolve(), args.suite.resolve()
    output = (args.output_root or root / "evaluations").resolve()
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    run_id = f"{stamp}-{uuid.uuid4().hex}"
    directory = output / f"{args.task}-{args.mode}-{run_id}"
    directory.mkdir(parents=True, exist_ok=False, mode=0o700)
    report_path = directory / f"nexus-{args.mode}.{run_id}.json"
    evidence = {"instance_id": args.task, "mode": args.mode, "evaluator_version": VERSION,
                "created_at_utc": stamp, "directory": str(directory), "run_id": run_id,
                "platform": "linux/amd64", "image_architecture": "x86_64",
                "official_report_path": str(report_path), "status": "preparing",
                "expected_outcome": {"gold": "resolved", "empty": "not-resolved"}.get(args.mode),
                "empty_patch_is_fail_to_pass_baseline": False,
                "empty_patch_note": "Official harness skips empty patches; this is not a true fail-to-pass baseline.",
                "input_sha256": {}}
    try:
        write_json(directory / "evaluation.json", evidence)
        suite = json.loads(suite_path.read_text())
        row, task = load_task(suite, root, args.task)
        scoring_image = verify_prepared_inputs(root, suite_path, row)
        dataset_config = (suite["dataset"] if isinstance(suite["dataset"], dict) else
                          {"name": suite["dataset"], "split": suite["split"], "revision": suite["revision"]})
        evidence.update(suite={"name": suite["name"], "dataset": dataset_config},
                        task=task, image=row["image"], scoring_image=scoring_image)
        source = root / f"{args.task}.task.json"
        evidence["input_sha256"].update(suite=sha256(suite_path), canonical_row=sha256(source))
        patch = row["patch"] if args.mode == "gold" else ""
        if args.mode == "prediction":
            if args.prediction is None:
                raise ValueError("--prediction is required in prediction mode")
            patch = prediction_patch(args.prediction, args.task)
            evidence["input_sha256"]["source_prediction"] = sha256(args.prediction)
        elif args.prediction is not None:
            raise ValueError("--prediction is only valid in prediction mode")
        dataset, predictions = directory / "dataset.json", directory / "predictions.jsonl"
        # Only the execution image reference changes; its content is pinned to
        # the prepared official digest. Gold/tests remain canonical and hashed.
        write_json(dataset, [{**row, "image": scoring_image["digest"]}])
        predictions.write_text(json.dumps({"instance_id": args.task, "model_patch": patch,
                                          "model_name_or_path": f"nexus-{args.mode}"}) + "\n")
        for path in (dataset, predictions):
            path.chmod(0o444)
            evidence["input_sha256"][path.name] = sha256(path)
        version_command = UV + ["-c", "from importlib.metadata import version; print(version('swebench'))"]
        command = UV + ["-m", "swebench.harness.run_evaluation", "--dataset_name", str(dataset),
                        "--split", dataset_config["split"], "--instance_ids", args.task,
                        "--predictions_path", str(predictions), "--max_workers", "1",
                        "--timeout", str(TEST_TIMEOUT), "--run_id", run_id,
                        "--report_dir", str(directory), "--modal", "false"]
        evidence.update(command=command, version_command=version_command, status="running",
                        process_timeout_seconds=PROCESS_TIMEOUT, test_timeout_seconds=TEST_TIMEOUT)
        write_json(directory / "evaluation.json", evidence)
        if run_command(version_command, directory, directory / "version.log") != 0:
            raise RuntimeError("pinned evaluator version check failed; see version.log")
        actual_version = (directory / "version.log").read_text().strip().splitlines()[-1]
        evidence["actual_evaluator_version"] = actual_version
        if actual_version != VERSION:
            raise RuntimeError("installed evaluator version mismatch")
        write_json(directory / "evaluation.json", evidence)
        evidence["returncode"] = run_command(command, directory, directory / "evaluator.log")
        # A partial report remains raw evidence, but never overrides process failure.
        if report_path.exists():
            evidence["official_report_sha256"] = sha256(report_path)
            evidence["evaluation_outcome"] = classify_report(json.loads(report_path.read_text()), args.task)
        if evidence["returncode"] != 0:
            raise RuntimeError("official evaluator exited unsuccessfully; see evaluator.log")
        if not report_path.exists():
            raise RuntimeError("official evaluator did not produce the expected report")
        for path in (dataset, predictions):
            if sha256(path) != evidence["input_sha256"][path.name]:
                raise RuntimeError("evaluation input changed during execution")
        outcome = evidence["evaluation_outcome"]
        if outcome not in ("resolved", "unresolved", "empty-patch"):
            raise RuntimeError("official evaluation is invalid/incomplete: " + outcome)
        if (patch == "") != (outcome == "empty-patch"):
            raise ValueError("official report empty-patch outcome contradicts submitted patch")
        expected = (outcome == "resolved" if args.mode == "gold" else
                    outcome in ("empty-patch", "unresolved") if args.mode == "empty" else None)
        evidence.update(status="completed", expectation_met=expected)
        return directory, 0 if expected is not False and outcome in ("resolved", "unresolved", "empty-patch") else 1
    except subprocess.TimeoutExpired:
        evidence.update(status="timeout", error="host process timeout; inspect Docker for remaining containers")
        return directory, 1
    except KeyboardInterrupt:
        evidence.update(status="interrupted", error="interrupted; inspect Docker for remaining containers")
        return directory, 130
    except (OSError, ValueError, KeyError, IndexError, TypeError, RuntimeError) as error:
        evidence.update(status="error", error=str(error))
        return directory, 1
    finally:
        evidence["finished_at_utc"] = datetime.now(timezone.utc).isoformat()
        write_json(directory / "evaluation.json", evidence)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work-root", type=Path,
                        default=HERE.parent.parent.parent / "nexus-benchmark-work/lite-diagnostics-v1")
    parser.add_argument("--suite", type=Path, default=HERE / "swe_suite.json")
    parser.add_argument("--task", required=True)
    parser.add_argument("--mode", choices=("gold", "empty", "prediction"), required=True)
    parser.add_argument("--prediction", type=Path)
    parser.add_argument("--output-root", type=Path)
    args = parser.parse_args(argv)
    # Prevent path traversal before constructing the evidence directory.
    if not re.fullmatch(r"[A-Za-z0-9_.-]+__[A-Za-z0-9_.-]+-\d+", args.task):
        parser.error("invalid task instance ID")
    def interrupted(signum, frame):
        raise KeyboardInterrupt
    previous = signal.signal(signal.SIGTERM, interrupted)
    try:
        directory, code = evaluate(args)
    finally:
        signal.signal(signal.SIGTERM, previous)
    print(directory)
    return code


if __name__ == "__main__":
    sys.exit(main())
