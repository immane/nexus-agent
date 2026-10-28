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
        return subprocess.run([str(SCRIPT)] + ([kind] if kind else []), input=answers, text=True,
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
        self.assertIn("Existing provider IDs:\n  #  |  ID", result.stdout)
        self.assertIn("  1  |  p", result.stdout)
        result = self.run_tool("model", "m\np\nx\n\n\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Existing model IDs:", result.stdout)
        self.assertIn("  (none)", result.stdout)
        result = self.run_tool("model", "1\n1\n\n\n\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Existing model IDs:\n  #  |  ID", result.stdout)
        self.assertIn("  1  |  m", result.stdout)

    def test_remote_and_local_default_models(self):
        requests = []
        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                requests.append((self.path, self.headers.get("Authorization"), self.headers.get("User-Agent")))
                if self.headers.get("User-Agent") != "nexus-agent-config/0.2":
                    self.send_error(403)
                    return
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
        self.assertEqual(requests, [("/v1/models", "Bearer test-token", "nexus-agent-config/0.2")])
        self.assertEqual(json.loads(self.path.read_text())["providers"][0]["default_model"], "remote-model")
        result = self.run_tool("model", "m\n1\nlocal-model\n\n\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        result = self.run_tool("provider", "1\n\n\n\n\n\n\n1\n\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('1  |  local-model', result.stdout)
        self.assertIn('2  |  remote-model', result.stdout)
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

    def test_menu_preset_creates_selectable_model_without_exposing_credentials(self):
        result = self.run_tool(None, "1\n1\n\n\nenv\nAPI_KEY\ndeepseek-chat\n\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        config = json.loads(self.path.read_text())
        self.assertEqual(config["providers"][0]["endpoint"], "https://api.deepseek.com")
        self.assertEqual(config["providers"][0]["protocol"], "chat_completions")
        self.assertEqual(config["models"], [{"id": "deepseek-chat", "provider": "deepseek", "name": "deepseek-chat"}])
        result = self.run_tool(None, "7\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("deepseek-chat", result.stdout)
        self.assertNotIn("API_KEY", result.stdout)
        self.assertIn("MANAGE\n\n  Providers                 Models", result.stdout)
        self.assertIn("[1] Add", result.stdout)
        self.assertIn("  > Select action [7]:", result.stdout)
        self.assertNotIn("|  Action", result.stdout)

    def test_menu_delete_cascades_only_after_confirmation(self):
        self.create_provider()
        config = json.loads(self.path.read_text())
        config.update(models=[{"id": "m", "provider": "p", "name": "x"}],
                      favourites=["m"], recent=["m"])
        self.path.write_text(json.dumps(config))
        before = self.path.read_bytes()
        result = self.run_tool(None, "3\n1\nno\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Favourites to remove: m", result.stdout)
        self.assertIn("Recent to remove: m", result.stdout)
        self.assertEqual(self.path.read_bytes(), before)
        result = self.run_tool(None, "3\n1\nyes\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        config = json.loads(self.path.read_text())
        for field in ("providers", "models", "favourites", "recent"):
            self.assertEqual(config[field], [])

    def test_menu_deletion_retains_shared_stored_credential(self):
        self.create_provider()
        self.env["XDG_CONFIG_HOME"] = self.directory.name
        secrets = Path(self.directory.name) / "nexus-agent" / "credentials.json"
        secrets.parent.mkdir()
        secrets.write_text('{"shared":"private-test-key"}')
        config = json.loads(self.path.read_text())
        config["providers"][0]["credential"] = {"stored": "shared"}
        second = dict(config["providers"][0], id="p2")
        config["providers"].append(second)
        self.path.write_text(json.dumps(config))
        result = self.run_tool(None, "3\n1\nyes\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("shared and will be retained", result.stdout)
        self.assertNotIn("private-test-key", result.stdout)
        self.assertEqual(json.loads(secrets.read_text()), {"shared": "private-test-key"})

    def test_menu_model_delete_cascades_references(self):
        self.create_provider()
        config = json.loads(self.path.read_text())
        config.update(models=[{"id": "m", "provider": "p", "name": "x"}],
                      favourites=["m"], recent=["m"])
        self.path.write_text(json.dumps(config))
        result = self.run_tool(None, "6\n1\nyes\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        config = json.loads(self.path.read_text())
        self.assertEqual(len(config["providers"]), 1)
        self.assertEqual((config["models"], config["favourites"], config["recent"]), ([], [], []))

    def test_opencode_preset_endpoint(self):
        result = self.run_tool(None, "1\n2\n\n\nenv\nAPI_KEY\nmodel\n\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        provider = json.loads(self.path.read_text())["providers"][0]
        self.assertEqual(provider["endpoint"], "https://opencode.ai/inference/go/openai/v1")
        self.assertEqual(provider["protocol"], "chat_completions")

    def test_opencode_public_model_discovery_does_not_send_key(self):
        shim = Path(self.directory.name) / "sitecustomize.py"
        record = Path(self.directory.name) / "request.json"
        shim.write_text('''import io, json, os, urllib.request
class Opener:
    def open(self, request, timeout):
        with open(os.environ["REQUEST_RECORD"], "w") as out:
            json.dump({"url": request.full_url, "headers": dict(request.header_items())}, out)
        return io.BytesIO(b'{"data":[{"id":"available-model"}]}')
urllib.request.build_opener = lambda *args: Opener()
''')
        self.env.update(PYTHONPATH=self.directory.name, REQUEST_RECORD=str(record), API_KEY="private-test-key")
        result = self.run_tool(None, "1\n2\n\n\nenv\nAPI_KEY\n1\n\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        request = json.loads(record.read_text())
        self.assertEqual(request["url"], "https://opencode.ai/inference/go/v1/models")
        self.assertNotIn("private-test-key", json.dumps(request))
        self.assertEqual(json.loads(self.path.read_text())["providers"][0]["default_model"], "available-model")

    def test_additional_presets_use_correct_protocol(self):
        for choice, endpoint, protocol in [
            ("5", "https://api.openai.com/v1", "chat_completions"),
            ("6", "https://api.anthropic.com", "anthropic_messages"),
            ("7", "https://openrouter.ai/api/v1", "chat_completions"),
        ]:
            with self.subTest(choice=choice):
                self.path.unlink(missing_ok=True)
                result = self.run_tool(None, f"1\n{choice}\n\n\nenv\nAPI_KEY\nmodel\n\n")
                self.assertEqual(result.returncode, 0, result.stderr)
                provider = json.loads(self.path.read_text())["providers"][0]
                self.assertEqual((provider["endpoint"], provider["protocol"]), (endpoint, protocol))

    def test_menu_escapes_terminal_control_characters(self):
        self.create_provider()
        config = json.loads(self.path.read_text())
        config["providers"][0]["display_name"] = "unsafe\u001b[2J"
        config["models"] = [{"id": "m", "provider": "p", "name": "test\u001b[2J"}]
        self.path.write_text(json.dumps(config))
        result = self.run_tool(None, "7\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("\x1b", result.stdout)
        self.assertIn("\\u001b", result.stdout)

    def test_delete_credential_requires_separate_confirmation(self):
        self.create_provider()
        self.env["XDG_CONFIG_HOME"] = self.directory.name
        secrets = Path(self.directory.name) / "nexus-agent" / "credentials.json"
        secrets.parent.mkdir()
        secrets.write_text('{"p":"private-test-key","other":"other-key"}')
        config = json.loads(self.path.read_text())
        config["providers"][0]["credential"] = {"stored": "p"}
        self.path.write_text(json.dumps(config))
        result = self.run_tool(None, "3\n1\nyes\nyes\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(secrets.read_text()), {"other": "other-key"})
        self.assertNotIn("private-test-key", result.stdout + result.stderr)

    def test_quick_edit_preserves_provider_options_without_secret_prompt(self):
        self.create_provider()
        config = json.loads(self.path.read_text())
        config["models"] = [{"id": "m", "provider": "p", "name": "x"}]
        before = config["providers"][0].copy()
        self.path.write_text(json.dumps(config))
        result = self.run_tool(None, "2\n1\n\n\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("Credential storage", result.stdout)
        self.assertEqual(json.loads(self.path.read_text())["providers"][0], before)


if __name__ == "__main__":
    unittest.main()
