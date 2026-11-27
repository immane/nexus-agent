"""Offline tests for the opt-in live context-endurance regression helpers."""

import json
from pathlib import Path
import tempfile
import unittest

from live_context_endurance import (
    LOCAL_CONTEXT_BYTES,
    big_output_prompt,
    parse_diagnostics,
    session_artifacts,
)


LOG = """\
nexus-runtime: context budget run=r-1 turn=t-1-0 stage=dispatch purpose=Task window_tokens=Some(1000000) configured_max_output_tokens=None planning_output_reserve_tokens=Some(16384) request_max_output_tokens=Some(16384) raw_estimated_input_tokens=237430 estimated_input_tokens=296788 estimate_scale_millis=1250 framing_tokens=15104 payload_bytes=BudgetPayloadBytes { instructions: 769, context_note: 186, tool_definitions: 3546, user_text: 1597, assistant_text: 35614, reasoning: 209585, arguments: 98604, tool_results: 282465, derived_memory: 0, retained_evidence: 0, identities: 34610 } measurement=heuristic_not_provider_usage
nexus-runtime: context reduced: removed_items=0 retained_items=470 max_items=8192 retained_bytes=500000 max_bytes=1048576
nexus-runtime: run finished run=r-1 outcome=Completed error_category=None source_persistence=Saved turns=5 calls=5 elapsed_ms=43443 output_limit_recovery_used=false data_dropped=false terminal_delivery=Sent
nexus-runtime: context budget exhausted: items=464 max_items=8192 bytes=1048899 max_bytes=1048576 continuation=0
"""


class DiagnosticsTests(unittest.TestCase):
    def test_parses_estimates_reductions_finish_and_exhaustion(self):
        value = parse_diagnostics(LOG)
        self.assertEqual(value["budgets"][0]["window"], 1_000_000)
        self.assertEqual(value["budgets"][0]["estimated_input"], 296_788)
        self.assertEqual(value["reductions"], [(0, 470, 8192, 500_000, LOCAL_CONTEXT_BYTES)])
        self.assertEqual(value["finished"][0]["outcome"], "Completed")
        self.assertEqual(value["finished"][0]["turns"], 5)
        self.assertEqual(value["exhausted"], [(464, 8192, 1_048_899, LOCAL_CONTEXT_BYTES, 0)])

    def test_ignores_unrelated_output_and_absent_values(self):
        value = parse_diagnostics("no diagnostics here\n")
        self.assertEqual(value, {"reductions": [], "exhausted": [], "budgets": [], "finished": []})

    def test_prompt_names_the_exact_command_and_call_count(self):
        prompt = big_output_prompt(180_000, 6)
        self.assertIn("host_exec", prompt)
        self.assertIn("head -c 180000 /dev/zero", prompt)
        self.assertIn("Call the host_exec tool 6 times", prompt)
        self.assertIn("BIG OUTPUT DONE", prompt)


class ArtifactTests(unittest.TestCase):
    def test_reads_completed_snapshot_and_operation_journal_without_credentials(self):
        with tempfile.TemporaryDirectory() as directory:
            store = Path(directory)
            sessions = store / "sessions"
            sessions.mkdir()
            session = "sess-live-test"
            name = session.encode().hex()
            item = lambda text, kind="tool_result", effect="unknown": {
                "kind": kind, "content": text, "effect": effect, "args": "{}",
                "item_key": "item", "provider_ref": "ref", "run": "run",
                "tool": "host_exec", "call": "call", "turn": "turn",
            }
            (store / "session-store.json").write_text(json.dumps({
                "source_records": [
                    {"run": "run", "sequence": 1, "item": item("a" * 100)},
                    {"run": "run", "sequence": 2, "item": item("b" * 200, "assistant_text", None)},
                ],
            }))
            (store / "session-store.operations.jsonl").write_text(
                json.dumps({"kind": "source_batch", "record": [
                    {"run": "run", "sequence": 3, "item": item("c" * 300)},
                ]}) + "\n"
            )
            # A directory-style body must not duplicate the same journal records.
            (sessions / f"{name}.json").write_text(json.dumps({"source_records": []}))
            value = session_artifacts(store, session)
            self.assertEqual(value["records"], 3)
            self.assertEqual(value["tool_results"], 2)
            self.assertEqual(value["unknown_effects"], 2)
            self.assertEqual(value["largest_result_bytes"], 300)
            self.assertGreater(value["payload_bytes"], 600)

    def test_missing_session_has_zero_history(self):
        with tempfile.TemporaryDirectory() as directory:
            value = session_artifacts(Path(directory), "sess-missing")
            self.assertEqual(value["records"], 0)
            self.assertEqual(value["payload_bytes"], 0)


if __name__ == "__main__":
    unittest.main()
