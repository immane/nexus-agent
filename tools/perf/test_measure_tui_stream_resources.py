#!/usr/bin/env python3
"""Deterministic tests for the finite local streaming payloads."""

import json
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from measure_tui_stream_resources import (  # noqa: E402
    coding_payload_chunks,
    coding_tool_call_events,
    coding_tool_result,
)


class CodingPayloadTests(unittest.TestCase):
    def test_payload_is_bounded_split_and_ends_in_expected_marker(self):
        chunks, payload_bytes = coding_payload_chunks(128)
        payload = "".join(chunks)

        self.assertEqual(len(chunks), 128)
        self.assertTrue(all(chunks))
        self.assertEqual(payload_bytes, len(payload.encode("utf-8")))
        self.assertGreater(payload_bytes, 4_000)
        self.assertLess(payload_bytes, 8_192)
        self.assertIn("```rust", payload)
        self.assertTrue(payload.endswith("// resource-000127\n"))

    def test_coding_tool_turn_is_read_only_and_followup_requires_result(self):
        frames = [json.loads(event[6:]) for event in coding_tool_call_events()
                  if event.startswith(b"data: {")]
        fragments = [choice["delta"]["tool_calls"][0]["function"].get("arguments", "")
                     for frame in frames
                     for choice in frame["choices"]
                     if choice["delta"].get("tool_calls")]
        self.assertEqual("".join(fragments), '{"path":"src/lib.rs"}')
        self.assertEqual(frames[-1]["choices"][0]["finish_reason"], "tool_calls")

        body = {"messages": [{"role": "tool", "tool_call_id": "fixture-read-1",
                              "content": "pub fn validate_target(path: &str) -> bool {}"}]}
        self.assertIn("validate_target", coding_tool_result(body))
        with self.assertRaisesRegex(AssertionError, "did not contain"):
            coding_tool_result({"messages": []})


if __name__ == "__main__":
    unittest.main()
