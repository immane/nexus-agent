"""Offline tests for benchmark result classification."""

from argparse import Namespace
from pathlib import Path
import subprocess
import tempfile
import unittest

from run_nexus_swe_task import reached_budget, summarize_result, validate_container_git_checkout


class SummaryTests(unittest.TestCase):
    def setUp(self):
        self.result = {
            "instance_id": "task-1",
            "outcome": "limit-reached",
            "error": {"category": "ResourceLimit"},
            "patch_bytes": 1028,
        }

    def test_resolved_patch_does_not_rewrite_agent_failure(self):
        report = {"submitted_ids": ["task-1"], "resolved_ids": ["task-1"]}
        summary = summarize_result(self.result, report)
        self.assertEqual(summary["agent_outcome"], "limit-reached")
        self.assertEqual(summary["agent_error"], self.result["error"])
        self.assertEqual(summary["evaluation_outcome"], "resolved")
        self.assertEqual(summary["infrastructure_outcome"], "valid")

    def test_missing_report_does_not_claim_scoring_success(self):
        self.assertEqual(summarize_result(self.result)["evaluation_outcome"], "not-run")

    def test_report_failures_remain_distinct(self):
        for field, expected in [
            ("infra_failure_ids", "infrastructure-failure"),
            ("ambiguous_failure_ids", "ambiguous"),
            ("error_ids", "error"),
            ("unresolved_ids", "unresolved"),
            ("empty_patch_ids", "empty-patch"),
            ("incomplete_ids", "incomplete"),
        ]:
            with self.subTest(field=field):
                report = {"submitted_ids": ["task-1"], field: ["task-1"]}
                self.assertEqual(summarize_result(self.result, report)["evaluation_outcome"], expected)

    def test_infrastructure_failure_takes_precedence_over_resolved(self):
        report = {
            "submitted_ids": ["task-1"],
            "resolved_ids": ["task-1"],
            "infra_failure_ids": ["task-1"],
        }
        self.assertEqual(summarize_result(self.result, report)["infrastructure_outcome"], "failed")

    def test_wrong_instance_report_is_rejected(self):
        with self.assertRaises(RuntimeError):
            summarize_result(self.result, {"submitted_ids": ["other-task"]})


class BudgetTests(unittest.TestCase):
    def test_unset_runner_limits_do_not_stop_a_run(self):
        args = Namespace(
            max_run_seconds=None,
            max_tool_calls=None,
            max_input_tokens=None,
            max_output_tokens=None,
        )
        self.assertIsNone(reached_budget(10**9, args, 10**6, {"input": 10**12, "output": 10**9}, False, 0))

    def test_configured_runner_limit_still_stops(self):
        args = Namespace(
            max_run_seconds=None,
            max_tool_calls=150,
            max_input_tokens=None,
            max_output_tokens=None,
        )
        self.assertEqual(
            reached_budget(1, args, 150, {"input": 0, "output": 0}, True, 1),
            "tool-call budget reached",
        )


class ContainerGitCheckoutTests(unittest.TestCase):
    def test_standalone_checkout_is_accepted(self):
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)
            subprocess.run(["git", "init", str(repo)], check=True, capture_output=True)
            validate_container_git_checkout(repo)

    def test_linked_worktree_git_file_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)
            (repo / ".git").write_text("gitdir: /host-only/worktree/.git\n")
            with self.assertRaisesRegex(RuntimeError, "standalone Git checkout"):
                validate_container_git_checkout(repo)

    def test_external_object_alternates_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)
            subprocess.run(["git", "init", str(repo)], check=True, capture_output=True)
            alternates = repo / ".git/objects/info/alternates"
            alternates.write_text("/host-only/git/objects\n")
            with self.assertRaisesRegex(RuntimeError, "object alternates"):
                validate_container_git_checkout(repo)


if __name__ == "__main__":
    unittest.main()
