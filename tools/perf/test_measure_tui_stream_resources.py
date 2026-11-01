#!/usr/bin/env python3
"""Deterministic tests for the finite local streaming payloads."""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from measure_tui_stream_resources import coding_payload_chunks  # noqa: E402


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


if __name__ == "__main__":
    unittest.main()
