"""Offline evaluator contract tests: no uv, Docker, datasets, or model calls."""

from argparse import Namespace
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch

import evaluate_swe_suite as evaluator

INSTANCE = "django__django-11099"


def official_report(outcome="resolved"):
    report = {"total_instances": 1, "schema_version": 2}
    for field in ("submitted", "completed", "resolved", "unresolved", "empty_patch", "error",
                  "incomplete", "infra_failure", "ambiguous_failure"):
        present = field == "submitted" or field == outcome or (
            field == "completed" and outcome in ("resolved", "unresolved"))
        report[field + "_ids"] = [INSTANCE] if present else []
        if field != "incomplete":
            report[field + "_instances"] = int(present)
    return report


class ReportTests(unittest.TestCase):
    def test_all_official_outcomes(self):
        for outcome in ("resolved", "unresolved", "empty_patch", "error", "incomplete"):
            with self.subTest(outcome=outcome):
                self.assertEqual(evaluator.classify_report(official_report(outcome), INSTANCE),
                                 outcome.replace("_", "-"))

    def test_infrastructure_annotations_are_not_task_success(self):
        for field in ("infra_failure", "ambiguous_failure"):
            report = official_report("unresolved")
            report[field + "_ids"] = [INSTANCE]
            report[field + "_instances"] = 1
            self.assertEqual(evaluator.classify_report(report, INSTANCE), field.replace("_", "-"))

    def test_reject_wrong_identity_denominators_and_conflicting_outcomes(self):
        mutations = [
            {"submitted_ids": ["other-task"]},
            {"resolved_ids": ["other-task"]},
            {"resolved_ids": [INSTANCE, INSTANCE], "resolved_instances": 2},
            {"total_instances": 2}, {"total_instances": True},
            {"submitted_instances": 0}, {"resolved_instances": 0},
            {"completed_ids": [], "completed_instances": 0},
            {"unresolved_ids": [INSTANCE], "unresolved_instances": 1},
            {"empty_patch_ids": [INSTANCE], "empty_patch_instances": 1},
            {"infra_failure_ids": [INSTANCE], "infra_failure_instances": 1},
            {"resolved_ids": [], "resolved_instances": 0},
        ]
        for mutation in mutations:
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                evaluator.classify_report(dict(official_report(), **mutation), INSTANCE)

    def test_missing_schema_fields_are_not_success(self):
        with self.assertRaises(ValueError):
            evaluator.classify_report({"resolved_ids": [INSTANCE]}, INSTANCE)


class EvaluationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.row = {
            "instance_id": INSTANCE, "repo": "django/django", "base_commit": "a" * 40,
            "version": "2.0", "problem_statement": "public issue", "patch": "gold patch\n",
            "test_patch": "hidden tests", "FAIL_TO_PASS": '["test_regression"]',
            "PASS_TO_PASS": "[]", "image": "swebench/sweb.eval.x86_64.django_1776_django-11099:latest",
            "eval_script": "echo official tests", "log_parser": "django", "eval_type": "unit",
            "extra_canonical_field": "preserve this too",
        }
        self.source = self.root / f"{INSTANCE}.task.json"
        self.source.write_text(json.dumps(self.row))
        self.suite = self.root / "suite.json"
        self.manifest = {
            "name": "fixture", "dataset": "SWE-bench/SWE-bench_Lite", "split": "test",
            "revision": "pinned-revision", "evaluator_version": "5.0.2", "platform": "linux/amd64",
            "defaults": {}, "tasks": [{"instance_id": INSTANCE, "domain": "web", "purpose": "test",
                                        "exposure": "previously exposed"}],
        }
        self.suite.write_text(json.dumps(self.manifest))
        self.digest = self.row["image"].split(":")[0] + "@sha256:" + "b" * 64
        self.record_preparation()
        inspector = patch.object(evaluator, "inspect_image", return_value={
            "Id": "sha256:" + "c" * 64, "Architecture": "amd64", "Os": "linux",
            "RepoDigests": [self.digest]})
        self.inspect_image = inspector.start()
        self.addCleanup(inspector.stop)
        self.args = Namespace(work_root=self.root, suite=self.suite, task=INSTANCE,
                              mode="gold", prediction=None, output_root=None)

    def record_preparation(self):
        (self.root / "suite-preparation.json").write_text(json.dumps({
            "suite_sha256": evaluator.sha256(self.suite), "tasks": {INSTANCE: {
                "task_sha256": evaluator.sha256(self.source), "images": {"base": {
                    "id": "sha256:" + "c" * 64, "repo_digests": [self.digest]}}}}}))

    def fake_commands(self, outcome="resolved", returncode=0, report=True):
        def run(argv, cwd, log):
            # Evidence exists before any process starts, including version setup.
            self.assertEqual(json.loads((cwd / "evaluation.json").read_text())["status"], "running")
            if log.name == "version.log":
                log.write_text("5.0.2\n")
                return 0
            log.write_text("official raw output\n")
            if report:
                path = Path(argv[argv.index("--report_dir") + 1])
                run_id = argv[argv.index("--run_id") + 1]
                (path / f"nexus-{self.args.mode}.{run_id}.json").write_text(json.dumps(official_report(outcome)))
            return returncode
        return run

    def run_evaluation(self, **kwargs):
        with patch.object(evaluator, "run_command", side_effect=self.fake_commands(**kwargs)) as run:
            directory, code = evaluator.evaluate(self.args)
        return directory, code, json.loads((directory / "evaluation.json").read_text()), run

    def test_gold_preserves_full_row_immutable_inputs_and_pinned_command(self):
        source_bytes = self.source.read_bytes()
        directory, code, evidence, run = self.run_evaluation()
        self.assertEqual(code, 0)
        self.assertEqual(evidence["status"], "completed")
        self.assertTrue(evidence["expectation_met"])
        self.assertEqual(evidence["actual_evaluator_version"], "5.0.2")
        self.assertEqual(json.loads((directory / "dataset.json").read_text()), [{**self.row, "image": self.digest}])
        prediction = json.loads((directory / "predictions.jsonl").read_text())
        self.assertEqual(prediction["model_patch"], self.row["patch"])
        self.assertEqual(self.source.read_bytes(), source_bytes)
        for name in ("dataset.json", "predictions.jsonl"):
            self.assertEqual((directory / name).stat().st_mode & 0o222, 0)
            self.assertEqual(evidence["input_sha256"][name], evaluator.sha256(directory / name))
        command = evidence["command"]
        self.assertEqual(command[:len(evaluator.UV)], evaluator.UV)
        self.assertIn("swebench==5.0.2", command)
        self.assertEqual(command[command.index("--max_workers") + 1], "1")
        self.assertEqual(command[command.index("--timeout") + 1], "1800")
        self.assertNotIn("--namespace", command)
        self.assertNotIn("--platform", command)
        self.assertEqual(run.call_count, 2)
        self.assertTrue(all(call.args[1] == directory for call in run.call_args_list))

    def test_empty_is_explicitly_skipped_not_a_fail_to_pass_baseline(self):
        self.args.mode = "empty"
        directory, code, evidence, _ = self.run_evaluation(outcome="empty_patch")
        self.assertEqual(code, 0)
        self.assertEqual(json.loads((directory / "predictions.jsonl").read_text())["model_patch"], "")
        self.assertEqual(evidence["evaluation_outcome"], "empty-patch")
        self.assertTrue(evidence["expectation_met"])
        self.assertFalse(evidence["empty_patch_is_fail_to_pass_baseline"])

    def test_gold_unresolved_is_failed_preflight(self):
        _, code, evidence, _ = self.run_evaluation(outcome="unresolved")
        self.assertEqual(code, 1)
        self.assertFalse(evidence["expectation_met"])

    def test_prediction_uses_saved_patch_without_inference(self):
        self.args.mode = "prediction"
        self.args.prediction = self.root / "saved.jsonl"
        self.args.prediction.write_text(json.dumps({"instance_id": INSTANCE, "model_patch": "saved patch"}) + "\n")
        directory, code, evidence, _ = self.run_evaluation(outcome="unresolved")
        self.assertEqual(code, 0)  # Valid scoring is not synonymous with resolved.
        self.assertEqual(json.loads((directory / "predictions.jsonl").read_text())["model_patch"], "saved patch")
        self.assertIsNone(evidence["expectation_met"])
        self.assertIn("source_prediction", evidence["input_sha256"])

    def test_bad_prediction_rejected_before_any_process(self):
        self.args.mode = "prediction"
        for value in (None, {"instance_id": "wrong", "model_patch": "patch"},
                      [{"instance_id": INSTANCE, "model_patch": "patch"}] * 2,
                      {"instance_id": INSTANCE, "model_patch": None}):
            if value is not None:
                self.args.prediction = self.root / "prediction.json"
                self.args.prediction.write_text(json.dumps(value))
            with patch.object(evaluator, "run_command") as run:
                directory, code = evaluator.evaluate(self.args)
                self.assertEqual(code, 1)
                self.assertEqual(json.loads((directory / "evaluation.json").read_text())["status"], "error")
                run.assert_not_called()

    def test_nonzero_exit_does_not_become_success_from_partial_report(self):
        _, code, evidence, run = self.run_evaluation(returncode=7)
        self.assertEqual(code, 1)
        self.assertEqual(evidence["status"], "error")
        self.assertEqual(evidence["returncode"], 7)
        self.assertIn("official_report_sha256", evidence)
        self.assertEqual(run.call_count, 2)  # Never retry.

    def test_missing_report_is_error(self):
        _, code, evidence, _ = self.run_evaluation(report=False)
        self.assertEqual(code, 1)
        self.assertEqual(evidence["status"], "error")

    def test_report_must_agree_with_submitted_patch_emptiness(self):
        for mode, outcome in (("gold", "empty_patch"), ("empty", "resolved")):
            self.args.mode = mode
            _, code, evidence, _ = self.run_evaluation(outcome=outcome)
            self.assertEqual(code, 1)
            self.assertEqual(evidence["status"], "error")

    def test_installed_version_mismatch_stops_before_evaluation(self):
        def wrong_version(argv, cwd, log):
            log.write_text("4.0.0\n")
            return 0
        with patch.object(evaluator, "run_command", side_effect=wrong_version) as run:
            directory, code = evaluator.evaluate(self.args)
        self.assertEqual(code, 1)
        self.assertEqual(run.call_count, 1)
        evidence = json.loads((directory / "evaluation.json").read_text())
        self.assertEqual(evidence["actual_evaluator_version"], "4.0.0")
        self.assertEqual(evidence["status"], "error")

    def test_wrong_task_report_is_preserved_but_rejected(self):
        real = self.fake_commands()
        def wrong_report(argv, cwd, log):
            code = real(argv, cwd, log)
            if log.name == "evaluator.log":
                path = Path(json.loads((cwd / "evaluation.json").read_text())["official_report_path"])
                report = json.loads(path.read_text())
                report["resolved_ids"] = ["other-task"]
                path.write_text(json.dumps(report))
            return code
        with patch.object(evaluator, "run_command", side_effect=wrong_report):
            directory, code = evaluator.evaluate(self.args)
        self.assertEqual(code, 1)
        evidence = json.loads((directory / "evaluation.json").read_text())
        self.assertEqual(evidence["status"], "error")
        self.assertNotIn("evaluation_outcome", evidence)
        self.assertTrue(Path(evidence["official_report_path"]).exists())

    def test_timeout_interruption_and_launch_error_persist_evidence(self):
        for error, status, expected_code in (
            (subprocess.TimeoutExpired(["uv"], 3600), "timeout", 1),
            (KeyboardInterrupt(), "interrupted", 130), (FileNotFoundError("uv"), "error", 1),
        ):
            with self.subTest(status=status), patch.object(evaluator, "run_command", side_effect=error) as run:
                directory, code = evaluator.evaluate(self.args)
                evidence = json.loads((directory / "evaluation.json").read_text())
                self.assertEqual(code, expected_code)
                self.assertEqual(evidence["status"], status)
                self.assertIn("command", evidence)
                self.assertIn("finished_at_utc", evidence)
                self.assertEqual(run.call_count, 1)

    def test_unique_directories(self):
        first, *_ = self.run_evaluation()
        second, *_ = self.run_evaluation()
        self.assertNotEqual(first, second)

    def test_nested_dataset_manifest_contract(self):
        self.manifest["dataset"] = {"name": self.manifest["dataset"],
                                    "split": self.manifest.pop("split"),
                                    "revision": self.manifest.pop("revision")}
        self.suite.write_text(json.dumps(self.manifest))
        self.record_preparation()
        _, code, evidence, _ = self.run_evaluation()
        self.assertEqual(code, 0)
        self.assertEqual(evidence["suite"]["dataset"], self.manifest["dataset"])

    def test_modified_canonical_metadata_blocks_before_evaluator(self):
        self.source.write_text(json.dumps(dict(self.row, patch="different gold")))
        with patch.object(evaluator, "run_command") as run:
            directory, code = evaluator.evaluate(self.args)
        self.assertEqual(code, 1)
        self.assertIn("metadata changed", json.loads((directory / "evaluation.json").read_text())["error"])
        run.assert_not_called()

    def test_retagged_image_blocks_before_evaluator(self):
        self.inspect_image.return_value["Id"] = "sha256:" + "d" * 64
        with patch.object(evaluator, "run_command") as run:
            directory, code = evaluator.evaluate(self.args)
        self.assertEqual(code, 1)
        self.assertIn("image differs", json.loads((directory / "evaluation.json").read_text())["error"])
        run.assert_not_called()

    def test_official_error_is_invalid_not_completed(self):
        _, code, evidence, _ = self.run_evaluation(outcome="error")
        self.assertEqual(code, 1)
        self.assertEqual(evidence["status"], "error")
        self.assertEqual(evidence["evaluation_outcome"], "error")

    def test_reject_stripped_row_wrong_suite_version_and_nonofficial_image(self):
        for mutation in ({"image": "local/adapter:latest"}, {"image": "swebench/sweb.eval.arm64.task:latest"}):
            self.source.write_text(json.dumps(dict(self.row, **mutation)))
            with self.assertRaises(ValueError):
                evaluator.load_task(self.manifest, self.root, INSTANCE)
        self.source.write_text(json.dumps({"instance_id": INSTANCE}))
        with self.assertRaises(ValueError):
            evaluator.load_task(self.manifest, self.root, INSTANCE)
        with self.assertRaises(ValueError):
            evaluator.load_task(dict(self.manifest, evaluator_version="4.0.0"), self.root, INSTANCE)

    def test_subprocess_platform_logs_and_group_cleanup(self):
        child = Mock(pid=12345)
        child.wait.side_effect = [subprocess.TimeoutExpired(["uv"], 3600), 0]
        with patch.object(evaluator.subprocess, "Popen", return_value=child) as popen, \
                patch.object(evaluator.os, "killpg") as kill:
            with self.assertRaises(subprocess.TimeoutExpired):
                evaluator.run_command(["uv", "fixture"], self.root, self.root / "process.log")
        self.assertEqual(popen.call_args.kwargs["env"]["DOCKER_DEFAULT_PLATFORM"], "linux/amd64")
        self.assertTrue(popen.call_args.kwargs["start_new_session"])
        kill.assert_called_once_with(12345, evaluator.signal.SIGTERM)


if __name__ == "__main__":
    unittest.main()
