#!/usr/bin/env python3
"""Regression tests for config.sh; requires nexus-server on PATH."""
import json
from http.server import BaseHTTPRequestHandler, HTTPServer
import os
import pty
from pathlib import Path
import select
import subprocess
import tempfile
import threading
import unittest

SCRIPT = Path(__file__).with_name("config.sh")


class ConfigScriptTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name) / "config.json"
        self.env = dict(os.environ, NEXUS_CONFIG=str(self.path))
        self.env.pop("API_KEY", None)

    def run_tool(self, kind, answers):
        return subprocess.run([str(SCRIPT), kind], input=answers, text=True,
                              env=self.env, capture_output=True)

    def create_provider(self):
        result = self.run_tool("provider", "p\nProvider\ndirect\nchat_completions\nhttps://example.com\nenv\nAPI_KEY\nx\n1000\n")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_update_preserves_policy_and_can_clear_fields(self):
        self.create_provider()
        config = json.loads(self.path.read_text())
        config["models"] = [{"id": "m", "provider": "p", "name": "x",
                             "context_management": {"strategy": "truncate"},
                             "context_window_tokens": 900, "max_output_tokens": 100}]
        self.path.write_text(json.dumps(config))
        result = self.run_tool("model", "m\n\n\n-\n-\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        model = json.loads(self.path.read_text())["models"][0]
        self.assertEqual(model["context_management"], {"strategy": "truncate"})
        self.assertNotIn("context_window_tokens", model)
        self.assertNotIn("max_output_tokens", model)
        result = self.run_tool("provider", "p\n\n\n\n-\n\n\n\n-\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIsNone(json.loads(self.path.read_text())["providers"][0]["endpoint"])
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o600)

    def test_invalid_resolved_window_does_not_write(self):
        self.create_provider()
        before = self.path.read_bytes()
        result = self.run_tool("model", "m\np\nx\n\n1000\n")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.path.read_bytes(), before)

    def test_existing_invalid_config_is_not_overwritten(self):
        self.path.write_text('{"revision":2,"providers":[],"models":[],"unknown":true}')
        before = self.path.read_bytes()
        result = self.run_tool("provider", "")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.path.read_bytes(), before)

    def test_provider_update_cannot_invalidate_existing_model(self):
        self.create_provider()
        result = self.run_tool("model", "m\np\nx\n\n900\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        before = self.path.read_bytes()
        result = self.run_tool("provider", "p\n\n\n\n\n\n\n\n500\n")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.path.read_bytes(), before)

    def test_duplicate_keys_are_rejected(self):
        self.path.write_text('{"revision":2,"revision":2,"providers":[],"models":[]}')
        before = self.path.read_bytes()
        result = self.run_tool("provider", "")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.path.read_bytes(), before)

    def test_existing_ids_are_shown_before_prompt(self):
        self.create_provider()
        result = self.run_tool("provider", "p\n\n\n\n\n\n\n\n\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Existing provider IDs:\n  1. p", result.stdout)
        result = self.run_tool("model", "m\np\nx\n\n\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Existing model IDs:\n  (none)", result.stdout)
        result = self.run_tool("model", "1\n1\n\n\n\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Existing model IDs:\n  1. m", result.stdout)

    def test_remote_and_local_default_models(self):
        requests = []
        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                requests.append((self.path, self.headers.get("Authorization")))
                body = b'{"data":[{"id":"remote-model"}]}'
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        server = HTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        self.env["API_KEY"] = "test-token"
        endpoint = f"http://127.0.0.1:{server.server_port}/v1"
        answers = f"p\nProvider\ndirect\nchat_completions\n{endpoint}\nenv\nAPI_KEY\n1\n\n"
        result = self.run_tool("provider", answers)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(requests, [("/v1/models", "Bearer test-token")])
        self.assertEqual(json.loads(self.path.read_text())["providers"][0]["default_model"], "remote-model")
        result = self.run_tool("model", "m\n1\nlocal-model\n\n\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        result = self.run_tool("provider", "1\n\n\n\n\n\n\n1\n\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('1. "local-model"', result.stdout)
        self.assertIn('2. "remote-model"', result.stdout)
        self.assertEqual(json.loads(self.path.read_text())["providers"][0]["default_model"], "local-model")

    def test_stored_key_is_hidden_and_private(self):
        self.env["XDG_CONFIG_HOME"] = self.directory.name
        master, slave = pty.openpty()
        process = subprocess.Popen([str(SCRIPT), "provider"], stdin=slave, stdout=slave,
                                   stderr=slave, env=self.env)
        os.close(slave)
        output = b""
        try:
            for prompt, answer in [
                (b"Provider ID", b"p\n"), (b"Display name", b"Provider\n"),
                (b"Adapter", b"direct\n"), (b"Protocol", b"chat_completions\n"),
                (b"Endpoint URL", b"\n"), (b"Credential storage", b"stored\n"),
                (b"API key", b"private-test-key\n"), (b"Default model", b"x\n"),
                (b"context window tokens", b"\n"),
            ]:
                received = b""
                while prompt not in received:
                    ready, _, _ = select.select([master], [], [], 10)
                    self.assertTrue(ready, f"prompt not received: {prompt!r}")
                    chunk = os.read(master, 4096)
                    self.assertTrue(chunk)
                    received += chunk
                    output += chunk
                os.write(master, answer)
            process.wait(timeout=10)
            self.assertEqual(process.returncode, 0, output.decode(errors="replace"))
            self.assertNotIn(b"private-test-key", output)
            secrets = Path(self.directory.name) / "nexus-agent" / "credentials.json"
            self.assertEqual(json.loads(secrets.read_text()), {"p": "private-test-key"})
            self.assertEqual(secrets.stat().st_mode & 0o777, 0o600)
            self.assertNotIn("private-test-key", self.path.read_text())
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            os.close(master)


if __name__ == "__main__":
    unittest.main()
