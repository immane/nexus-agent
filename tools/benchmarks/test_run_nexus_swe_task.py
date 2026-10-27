"""Offline tests for benchmark result classification."""

from argparse import Namespace
from pathlib import Path
import subprocess
import tempfile
import unittest

from run_nexus_swe_task import (
    build_task_prompt,
    context_usage_report,
    reached_budget,
    summarize_result,
    validate_container_git_checkout,
    tool_log_text,
)


class ToolLogTests(unittest.TestCase):
    def test_redacts_known_credentials_and_bearer_headers_before_truncation(self):
        import json
        value = json.loads(tool_log_text("token=secret-value Authorization: Bearer other-secret", 200, ["secret-value"]))
        self.assertNotIn("secret-value", value["preview"])
        self.assertNotIn("other-secret", value["preview"])
        self.assertIn("[REDACTED]", value["preview"])

    def test_utf8_preview_is_byte_bounded_and_marks_log_truncation(self):
        import json
        value = json.loads(tool_log_text("工具 output", 4, []))
        self.assertLessEqual(len(value["preview"].encode()), 4)
        self.assertTrue(value["log_truncated"])


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

    def test_token_budget_uses_known_reported_counters_as_soft_limit(self):
        args = Namespace(
            max_run_seconds=None,
            max_tool_calls=None,
            max_input_tokens=100,
            max_output_tokens=50,
        )
        self.assertIsNone(reached_budget(1, args, 0, {"input": 90, "output": 1}, False, 0))
        self.assertEqual(
            reached_budget(1, args, 0, {"input": 100, "output": 1}, False, 0),
            "input-token budget reached",
        )


class ContextUsageLogTests(unittest.TestCase):
    def test_reports_provider_usage_against_known_window(self):
        report = context_usage_report(61_236, 94_208, 65)
        self.assertIn("provider_input_tokens=61236", report)
        self.assertIn("context_percent=65.0", report)
        self.assertIn("provider_input_at_or_above_65pct=True", report)

    def test_unknown_usage_or_window_stays_unknown(self):
        self.assertIn("context_percent=unknown", context_usage_report(None, 94_208, 65))
        self.assertIn("context_window_tokens=unknown", context_usage_report(100, None, 65))


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


class TaskPromptTests(unittest.TestCase):
    def test_prompt_identifies_authoritative_workspace_and_project_test_docs(self):
        prompt = build_task_prompt("task-1", "abc123", "Fix the reported issue.")
        self.assertIn("mounted at /workspace", prompt)
        self.assertIn("authoritative checkout", prompt)
        self.assertIn("Do not treat other image paths such as /testbed", prompt)
        self.assertIn("follow the test instructions documented in this checkout", prompt)
        self.assertIn("do not assume a particular test framework", prompt)

    def test_prompt_stages_validation_before_justified_full_suite(self):
        prompt = build_task_prompt("task-1", "abc123", "Fix the reported issue.")
        self.assertIn("targeted regression first, then the related suite", prompt)
        self.assertIn("full suite only when justified", prompt)
        self.assertIn("remaining budget", prompt)

    def test_prompt_preserves_real_test_status_when_inspecting_output(self):
        prompt = build_task_prompt("task-1", "abc123", "Fix the reported issue.")
        self.assertIn("exact test process exit status", prompt)
        self.assertIn("tail/head", prompt)
        self.assertIn("capturing the test status before inspecting", prompt)
        self.assertIn("enable pipefail", prompt)
        self.assertIn("real test status from PIPESTATUS, not the filter's status", prompt)
        self.assertIn("report its timeout status and incomplete results separately", prompt)

    def test_prompt_distinguishes_progress_from_heartbeat_and_diagnoses_timeout(self):
        prompt = build_task_prompt("task-1", "abc123", "Fix the reported issue.")
        self.assertIn("progress log with test case names and results", prompt)
        self.assertIn("last started/completed case", prompt)
        self.assertIn("heartbeat means only that it is alive", prompt)
        self.assertIn("not that tests are making progress", prompt)
        self.assertIn("bounded execution deadline", prompt)
        self.assertIn("inspect the progress log and available stacks", prompt)
        self.assertIn("narrow the diagnostic run", prompt)
        self.assertIn("Do not repeat the same full-suite command unchanged", prompt)
        self.assertIn("a timeout is not a passing test result", prompt)

    def test_prompt_conditions_pytest_diagnostics_on_installed_support(self):
        prompt = build_task_prompt("task-1", "abc123", "Fix the reported issue.")
        self.assertIn("If this checkout uses pytest", prompt)
        self.assertIn("installed version supports the built-in faulthandler plugin", prompt)
        self.assertIn("-vv -o faulthandler_timeout=120", prompt)
        self.assertIn("interval below the execution deadline", prompt)
        self.assertIn("does not terminate a hung test", prompt)
        self.assertIn("Do not assume pytest-timeout or other uninstalled plugins", prompt)

    def test_prompt_forbids_weakening_tests_or_enabling_network(self):
        prompt = build_task_prompt("task-1", "abc123", "Fix the reported issue.")
        self.assertIn("Do not weaken tests, skip failing cases", prompt)
        self.assertIn("change expectations merely to obtain a pass", prompt)
        self.assertIn("Do not enable external network access", prompt)
        self.assertIn("or download dependencies/plugins", prompt)


if __name__ == "__main__":
    unittest.main()
