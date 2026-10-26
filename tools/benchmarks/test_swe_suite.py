"""Offline suite accounting and launcher environment checks."""

import json
import hashlib
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import swe_suite
import swebench
from prepare_swe_suite import EXPOSURE


INSTANCE = "pallets__flask-4992"


class SuiteTests(unittest.TestCase):
    def test_membership_selection_and_duplicates_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "suite.json"
            value = json.loads((swe_suite.HERE / "swe_suite.json").read_text())
            value["tasks"] = [{"instance_id": INSTANCE, "domain": "web", "purpose": "config",
                               "exposure": EXPOSURE}]
            path.write_text(json.dumps(value))
            self.assertEqual(swe_suite.read_suite(path, [INSTANCE])[1], value["tasks"])
            with self.assertRaisesRegex(ValueError, "not in the suite"):
                swe_suite.read_suite(path, ["other__repo-1"])
            value["tasks"] *= 2
            path.write_text(json.dumps(value))
            with self.assertRaisesRegex(ValueError, "duplicate"):
                swe_suite.read_suite(path)

    def test_missing_tasks_block_before_build_or_inference(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "suite-profiles.json").write_text('{"tasks": {}}')
            suite_path = root / "suite.json"
            suite_path.write_text("{}")
            (root / "suite-preparation.json").write_text(json.dumps({
                "suite_sha256": hashlib.sha256(suite_path.read_bytes()).hexdigest()}))
            args = SimpleNamespace(work_root=root, profiles=None, suite=suite_path)
            with patch.object(swebench, "available_tasks", return_value={}), \
                    patch.object(swebench, "command") as command, \
                    self.assertRaisesRegex(ValueError, "missing"):
                swe_suite.execute_batch(args, {}, [{"instance_id": INSTANCE}])
            command.assert_not_called()

    def test_unstarted_tasks_remain_in_denominator_and_unscored_is_not_resolved(self):
        summary = swe_suite.summarize({"suite": "test", "status": "interrupted-or-failed", "tasks": [
            {"classification": "completed-unscored", "official_resolved": None},
            {"classification": "unstarted"}]})
        self.assertEqual(summary["selected"], 2)
        self.assertEqual(summary["classifications"]["unstarted"], 1)
        self.assertIsNone(summary["tasks"][0]["official_resolved"])

    def test_result_and_cleanup_are_independent(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            run = root / "run-test"
            (run / "artifacts").mkdir(parents=True)
            launch = {"task": INSTANCE, "status": "finished", "cleanup": "complete"}
            result = {"instance_id": INSTANCE, "terminal_observed": True,
                      "outcome": "completed", "patch_bytes": 20}
            (run / "launch.json").write_text(json.dumps(launch))
            result_path = run / "artifacts" / f"{INSTANCE}.result.json"
            result_path.write_text(json.dumps(result))
            collected = swe_suite.collect_attempt(root, INSTANCE)
            self.assertEqual(collected["classification"], "completed-unscored")
            self.assertIsNone(collected["usage"])
            result["outcome"] = "limit-reached"
            result_path.write_text(json.dumps(result))
            self.assertEqual(swe_suite.collect_attempt(root, INSTANCE)["classification"], "agent-failed")
            launch["cleanup"] = "uncertain"
            (run / "launch.json").write_text(json.dumps(launch))
            self.assertEqual(swe_suite.collect_attempt(root, INSTANCE)["classification"], "cleanup-required")

    def test_missing_result_is_not_success_and_wrong_identity_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.assertEqual(swe_suite.collect_attempt(root, INSTANCE)["classification"], "preparation-failed")
            (root / "run-test").mkdir()
            (root / "run-test/launch.json").write_text(json.dumps({"task": "wrong"}))
            with self.assertRaisesRegex(ValueError, "does not match"):
                swe_suite.collect_attempt(root, INSTANCE)

    def test_atomic_save_replaces_batch_without_leaving_temp(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "batch.json"
            swe_suite.save_batch(path, {"status": "running"})
            swe_suite.save_batch(path, {"status": "finished"})
            self.assertEqual(json.loads(path.read_text())["status"], "finished")
            self.assertFalse(path.with_suffix(".tmp").exists())

    def test_prepare_batch_never_invokes_inference_and_keeps_task_data_safe(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            suite_path = swe_suite.HERE / "swe_suite.json"
            suite, tasks = swe_suite.read_suite(suite_path, [INSTANCE])
            row = {"instance_id": INSTANCE, "repo": "pallets/flask", "base_commit": "a" * 40,
                   "problem_statement": "Issue", "patch": "gold", "test_patch": "hidden"}
            source = root / f"{INSTANCE}.task.json"
            source.write_text(json.dumps(row))
            profile = root / "suite-profiles.json"
            profile.write_text('{"tasks":{}}')
            (root / "suite-preparation.json").write_text(json.dumps({
                "suite_sha256": hashlib.sha256(suite_path.read_bytes()).hexdigest(),
                "tasks": {INSTANCE: {"task_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
                                     "images": {"derived": {"id": "sha256:expected"},
                                                "build_source_sha256": swe_suite.build_source_sha256()}}}}))
            args = SimpleNamespace(work_root=root, profiles=None, suite=suite_path, build=False,
                                   action="prepare", config=root / "config.json", provider=None,
                                   model=None, model_output_tokens=None, max_run_seconds=None,
                                   max_tool_calls=None, tool_log_bytes=None)
            launcher_exit = 0

            def launch(argv, log):
                if args.action == "prepare":
                    self.assertIn("--prepare-only", argv)
                else:
                    self.assertNotIn("--prepare-only", argv)
                self.assertIn("--verify-task-environment", argv)
                run = log.parent / "run-fixture"
                run.mkdir()
                (run / "launch.json").write_text(json.dumps({
                    "task": INSTANCE, "status": "prepared", "cleanup": "complete"}))
                if args.action == "run":
                    (run / "artifacts").mkdir()
                    (run / "artifacts" / f"{INSTANCE}.result.json").write_text(json.dumps({
                        "instance_id": INSTANCE, "terminal_observed": True,
                        "outcome": "completed", "patch_bytes": 10}))
                return launcher_exit

            with patch.object(swebench, "available_tasks", return_value={INSTANCE: {"row": row, "image": "adapter"}}), \
                    patch.object(swebench, "inspect_image", return_value={"Id": "sha256:expected"}), \
                    patch.object(swe_suite, "run_command", side_effect=launch):
                self.assertEqual(swe_suite.execute_batch(args, suite, tasks), 0)
            batches = list(root.glob("suites/*/batch.json"))
            self.assertEqual(len(batches), 1)
            batch = json.loads(batches[0].read_text())
            self.assertEqual(batch["tasks"][0]["classification"], "prepared")
            copied = Path(batch["tasks"][0]["attempt_root"]) / source.name
            self.assertNotIn("patch", json.loads(copied.read_text()))
            self.assertNotIn("test_patch", json.loads(copied.read_text()))
            args.action = "run"
            launcher_exit = 1
            with patch.object(swebench, "available_tasks", return_value={INSTANCE: {"row": row, "image": "adapter"}}), \
                    patch.object(swebench, "inspect_image", return_value={"Id": "sha256:expected"}), \
                    patch.object(swe_suite, "run_command", side_effect=launch):
                self.assertEqual(swe_suite.execute_batch(args, suite, tasks), 1)
            failed = [json.loads(path.read_text()) for path in root.glob("suites/*/batch.json")
                      if json.loads(path.read_text())["action"] == "run"][0]
            self.assertEqual(failed["tasks"][0]["classification"], "launcher-failed")
            self.assertEqual(failed["tasks"][0]["agent_outcome"], "completed")
            launcher_exit = 130
            with patch.object(swebench, "available_tasks", return_value={INSTANCE: {"row": row, "image": "adapter"}}), \
                    patch.object(swebench, "inspect_image", return_value={"Id": "sha256:expected"}), \
                    patch.object(swe_suite, "run_command", side_effect=launch), \
                    self.assertRaisesRegex(RuntimeError, "batch stopped"):
                swe_suite.execute_batch(args, suite, tasks)
            self.assertTrue(any(json.loads(path.read_text())["status"] == "interrupted-or-failed"
                                for path in root.glob("suites/*/batch.json")))

    def test_gold_evaluation_never_claims_agent_completion(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            suite_path = swe_suite.HERE / "swe_suite.json"
            suite, tasks = swe_suite.read_suite(suite_path, [INSTANCE])
            args = SimpleNamespace(work_root=root, suite=suite_path, mode="gold", batch=None)

            def evaluate(options):
                self.assertIsNone(options.prediction)
                output = options.output_root / "oracle"
                output.mkdir(parents=True)
                (output / "evaluation.json").write_text(json.dumps({
                    "status": "completed", "evaluation_outcome": "resolved", "expectation_met": True}))
                return output, 0

            with patch.object(swe_suite.evaluate_swe_suite, "evaluate", side_effect=evaluate):
                self.assertEqual(swe_suite.evaluate_batch(args, suite, tasks), 0)
            batch = json.loads(next(root.glob("suites/*/batch.json")).read_text())
            self.assertTrue(batch["tasks"][0]["official_resolved"])
            self.assertNotIn("agent_attempt", batch["tasks"][0])


class EnvironmentTests(unittest.TestCase):
    def test_import_preflight_uses_offline_workload_and_records_unknown_tests(self):
        row = {"repo": "pallets/flask", "base_commit": "a" * 40}
        with patch.object(swebench, "command", side_effect=["a" * 40, "", '{"restored_files": 0}', "",
                                                           '{"source":"/workspace/src/flask/__init__.py"}']) as call:
            evidence = swebench.verify_task_environment({"workload_id": "b" * 64}, row)
        self.assertTrue(evidence["clean_checkout"])
        self.assertIn("not-run", evidence["public_tests"])
        argv = call.call_args.args[0]
        self.assertEqual(argv[:5], ["docker", "exec", "-w", "/workspace", "b" * 64])
        self.assertIn("'/workspace'", argv[-1])

    def test_wrong_revision_or_dirty_checkout_blocks_inference(self):
        row = {"repo": "pallets/flask", "base_commit": "a" * 40}
        for outputs in (["b" * 40], ["a" * 40, " M flask/config.py"]):
            with self.subTest(outputs=outputs), patch.object(swebench, "command", side_effect=outputs), \
                    self.assertRaisesRegex(RuntimeError, "not clean"):
                swebench.verify_task_environment({"workload_id": "b" * 64}, row)


if __name__ == "__main__":
    unittest.main()
