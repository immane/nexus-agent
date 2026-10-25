"""Offline launcher regressions; never invoke Docker, providers, or GitHub."""

import json
import io
from pathlib import Path
import tempfile
import sys
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import swebench


def row():
    return {"instance_id": "sympy__sympy-20590", "repo": "sympy/sympy",
            "base_commit": "a" * 40, "problem_statement": "Issue", "version": "1.7",
            "patch": "gold patch", "test_patch": "hidden tests", "eval_script": "hidden script"}


def owner():
    return {"nonce": "a" * 32, "gateway_name": "nexus-gw-" + "a" * 32,
            "gateway_id": "b" * 64, "workload_id": "c" * 64}


class DiscoveryTests(unittest.TestCase):
    def test_discovers_rows_and_arrays_but_does_not_scan_workspace(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "task.task.json").write_text(json.dumps(row()))
            (root / "pilot/official-evaluation").mkdir(parents=True)
            (root / "pilot/official-evaluation/dataset.json").write_text(json.dumps([row()]))
            (root / "pilot/workspace").mkdir()
            other = {**row(), "instance_id": "not-a-local-task"}
            (root / "pilot/workspace/task.json").write_text(json.dumps(other))
            tasks = swebench.discover_tasks(root)
            self.assertEqual(list(tasks), [row()["instance_id"]])

    def test_conflicting_task_identity_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "one.task.json").write_text(json.dumps(row()))
            (root / "two.task.json").write_text(json.dumps({**row(), "base_commit": "b" * 40}))
            with self.assertRaisesRegex(RuntimeError, "conflicting"):
                swebench.discover_tasks(root)

    def test_agent_input_excludes_gold_and_evaluator_data(self):
        value = swebench.agent_task(row())
        self.assertEqual(set(value), {"instance_id", "repo", "base_commit", "problem_statement", "version"})

    def test_rejects_untrusted_repo_and_revision(self):
        for field, value in (("instance_id", "../../private"), ("repo", "../../private"), ("base_commit", "--option")):
            with self.subTest(field=field), self.assertRaises(RuntimeError):
                swebench.agent_task({**row(), field: value})

    def test_cached_adapter_is_required_and_selected_in_profile_order(self):
        profiles = {"tasks": {row()["instance_id"]: {"images": ["new:pilot", "old:pilot"]}}}
        with patch.object(swebench, "command", return_value="old:pilot\nnew:pilot"), \
                patch.object(swebench, "discover_tasks", return_value={row()["instance_id"]: {"row": row()}}):
            tasks = swebench.available_tasks(Path("unused"), profiles)
        self.assertEqual(tasks[row()["instance_id"]]["image"], "new:pilot")


class NetworkTests(unittest.TestCase):
    def test_disconnects_exact_gateway_and_checks_routes_before_inference(self):
        gateway = {"NetworkSettings": {"Networks": {"bridge": {}}}}
        disconnected = {"NetworkSettings": {"Networks": {}}}
        with patch.object(swebench, "verify_owned", side_effect=[(gateway, {}), (disconnected, {})]), \
                patch.object(swebench, "command", side_effect=["", "Iface Destination Gateway", "head1\nhead2\n lo: 0\n gre0: 0", "1: lo: <LOOPBACK,UP> mtu 65536"]) as commands:
            evidence = swebench.disconnect_network(owner(), Path("workspace"))
        self.assertTrue(evidence["verified"])
        self.assertEqual(evidence["up_interfaces"], ["lo"])
        self.assertEqual(commands.call_args_list[0].args[0], ["docker", "network", "disconnect", "bridge", "b" * 64])

    def test_network_attachment_failure_blocks_submission(self):
        gateway = {"NetworkSettings": {"Networks": {"bridge": {}}}}
        with patch.object(swebench, "verify_owned", return_value=(gateway, {})), \
                patch.object(swebench, "command", return_value=""), \
                self.assertRaisesRegex(RuntimeError, "network attachments"):
            swebench.disconnect_network(owner(), Path("workspace"))

    def test_active_interface_or_route_fails_closed(self):
        for routes, links in (("Iface\neth0 00000000", "1: lo: <UP>"),
                              ("Iface", "1: lo: <UP>\n2: eth0: <UP>")):
            with self.subTest(routes=routes, links=links), \
                    patch.object(swebench, "verify_owned", return_value=({"NetworkSettings": {"Networks": {}}}, {})), \
                    patch.object(swebench, "command", side_effect=[routes, "head1\nhead2\n lo: 0", links]), \
                    self.assertRaisesRegex(RuntimeError, "active interfaces/routes"):
                swebench.disconnect_network(owner(), Path("workspace"))


class CleanupTests(unittest.TestCase):
    def test_cleanup_removes_workload_before_gateway_and_only_exact_ids(self):
        record = owner()
        workspace = Path("workspace").resolve()
        worker = {"Id": record["workload_id"],
                  "Config": {"Labels": {"io.nexus-agent.workload-owner": "workload-" + record["nonce"]}},
                  "Mounts": [{"Type": "bind", "Source": str(workspace), "Destination": "/workspace"}]}
        gateway = {"Id": record["gateway_id"],
                   "Config": {"Labels": {"io.nexus-agent.gateway-owner": record["gateway_name"]}}}
        outputs = [record["workload_id"] + "\n" + record["gateway_id"], json.dumps([worker]), "", json.dumps([gateway]), ""]
        with patch.object(swebench, "command", side_effect=outputs) as commands:
            swebench.cleanup_containers(record, workspace)
        deletes = [call.args[0] for call in commands.call_args_list if "rm" in call.args[0]]
        self.assertEqual(deletes, [["docker", "rm", "-f", record["workload_id"]],
                                  ["docker", "rm", "-f", record["gateway_id"]]])

    def test_already_removed_containers_do_not_trigger_global_cleanup(self):
        with patch.object(swebench, "command", return_value="") as commands:
            swebench.cleanup_containers(owner(), Path("workspace"))
        self.assertEqual(commands.call_count, 1)

    def test_cleanup_rejects_foreign_label_without_deleting(self):
        info = {"Id": "c" * 64, "Config": {"Labels": {}}}
        with patch.object(swebench, "command", side_effect=["c" * 64, json.dumps([info])]) as commands, \
                self.assertRaisesRegex(RuntimeError, "mismatched ownership"):
            swebench.cleanup_containers(owner(), Path("workspace"))
        self.assertFalse(any("rm" in call.args[0] for call in commands.call_args_list))

    def test_mismatched_mount_blocks_cleanup(self):
        info = {"Id": "c" * 64, "Config": {"Labels": {"io.nexus-agent.workload-owner": "workload-" + "a" * 32}},
                "Mounts": [{"Type": "bind", "Source": "/another-workspace", "Destination": "/workspace"}]}
        with patch.object(swebench, "command", side_effect=["c" * 64, json.dumps([info])]), \
                self.assertRaisesRegex(RuntimeError, "mismatched workspace"):
            swebench.cleanup_containers(owner(), Path("workspace"))


class ModelPreflightTests(unittest.TestCase):
    def test_ollama_preflight_does_not_require_or_send_a_token(self):
        provider = {"endpoint": "http://127.0.0.1:11434/v1", "credential": {"env": "OLLAMA_API_KEY"}}
        with patch.object(swebench, "build_opener") as opener:
            opener.return_value.open.return_value = io.StringIO('{"data":[{"id":"local-model"}]}')
            swebench.check_model(provider, {"name": "local-model"}, {})
        req = opener.return_value.open.call_args.args[0]
        self.assertFalse(req.has_header("Authorization"))

    def test_model_identity_check_makes_no_generation_request(self):
        provider = {"endpoint": "http://127.0.0.1:11434/v1", "credential": {"env": "TEST_TOKEN"}}
        with patch.object(swebench, "build_opener") as opener:
            opener.return_value.open.return_value = io.StringIO('{"data":[{"id":"local-model"}]}')
            swebench.check_model(provider, {"name": "local-model"}, {"TEST_TOKEN": "dummy"})
        req = opener.return_value.open.call_args.args[0]
        self.assertEqual(req.full_url, "http://127.0.0.1:11434/v1/models")
        self.assertEqual(req.get_method(), "GET")

    def test_wrong_model_blocks_launch(self):
        provider = {"endpoint": "http://127.0.0.1:11434/v1", "credential": {"env": "TEST_TOKEN"}}
        with patch.object(swebench, "build_opener") as opener:
            opener.return_value.open.return_value = io.StringIO('{"data":[]}')
            with self.assertRaisesRegex(RuntimeError, "not listed"):
                swebench.check_model(provider, {"name": "missing"}, {"TEST_TOKEN": "dummy"})


class OutputTests(unittest.TestCase):
    def test_tee_keeps_exact_log_and_console_bytes(self):
        output = "model partial response\n工具 output\n".encode()
        log, console = io.BytesIO(), io.BytesIO()
        swebench.tee_output(io.BytesIO(output), log, console)
        self.assertEqual(log.getvalue(), output)
        self.assertEqual(console.getvalue(), output)

    def test_broken_console_does_not_stop_disk_logging(self):
        class ClosedConsole:
            def write(self, _chunk):
                raise BrokenPipeError

        output = b"x" * 40000
        log = io.BytesIO()
        swebench.tee_output(io.BytesIO(output), log, ClosedConsole())
        self.assertEqual(log.getvalue(), output)

    def test_child_output_is_visible_and_persisted_before_exit(self):
        received = threading.Event()

        class Console(io.BytesIO):
            def write(self, chunk):
                result = super().write(chunk)
                received.set()
                return result

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "model-output.log"
            console = Console()
            with patch.object(swebench.sys, "stdout", SimpleNamespace(buffer=console)):
                child, pump, errors = swebench.logged_process(
                    [sys.executable, "-u", "-c", "import sys,time; sys.stdout.write('partial'); sys.stdout.flush(); time.sleep(30)"],
                    None, path)
                try:
                    self.assertTrue(received.wait(timeout=5), "output was buffered until process exit")
                    self.assertIsNone(child.poll())
                    self.assertEqual(path.read_bytes(), b"partial")
                    self.assertEqual(console.getvalue(), b"partial")
                finally:
                    child.terminate()
                    child.wait(timeout=5)
                    pump.join(timeout=5)
            self.assertFalse(pump.is_alive())
            self.assertEqual(errors, [])


class ArchiveLaunchTests(unittest.TestCase):
    def test_launcher_uses_a_run_local_archive_outside_the_workload_mount(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            server = root / "target/release/nexus-server"
            server.parent.mkdir(parents=True)
            server.write_bytes(b"fixture")
            config = root / "config.json"
            config.write_text(json.dumps({
                "providers": [{"id": "ollama-local", "endpoint": "http://127.0.0.1:11434/v1",
                               "credential": {"env": "OLLAMA_API_KEY"}}],
                "models": [{"id": "local-model", "provider": "ollama-local"}],
            }))
            profiles = {"provider": "ollama-local", "model": "local-model", "gateway_image": "gateway"}
            profile_path = root / "profiles.json"
            profile_path.write_text(json.dumps(profiles))
            args = SimpleNamespace(
                config=config, profiles=profile_path, model=None, provider=None,
                model_output_tokens=32768,
                gateway_image=None, container_image=None, build=False,
                work_root=root / "runs", task=row()["instance_id"], port=0,
                max_tool_calls=4096, max_run_seconds=86400,
            )
            task = {"row": row(), "image": "workload", "source": "fixture", "exposure": "exposed"}
            with patch.object(swebench, "REPO", root), \
                    patch.object(swebench, "check_model"), \
                    patch.object(swebench, "inspect_image", return_value={"Id": "fixture", "Architecture": "arm64"}), \
                    patch.object(swebench, "command", return_value=row()["base_commit"]), \
                    patch.object(swebench.socket, "socket") as socket, \
                    patch.object(swebench, "logged_process", side_effect=RuntimeError("stop-before-server")) as spawn, \
                    patch.object(swebench.sys, "stdout", io.StringIO()):
                socket.return_value.__enter__.return_value.getsockname.return_value = ("127.0.0.1", 12345)
                with self.assertRaisesRegex(RuntimeError, "stop-before-server"):
                    swebench.launch(args, profiles, task)
            run = next(args.work_root.iterdir())
            argv = spawn.call_args.args[0]
            archive = Path(argv[argv.index("--session-store") + 1])
            self.assertEqual(archive, (run / "session-store").resolve())
            self.assertNotIn("workspace", archive.parts)
            self.assertEqual(json.loads((run / "launch.json").read_text())["server_argv"], argv)
            effective = Path(argv[argv.index("--config") + 1])
            self.assertEqual(effective, run / "model-config.json")
            self.assertEqual(json.loads(effective.read_text())["models"][0]["max_output_tokens"], 32768)
            self.assertNotIn("max_output_tokens", json.loads(config.read_text())["models"][0])
            self.assertEqual(effective.stat().st_mode & 0o777, 0o600)


if __name__ == "__main__":
    unittest.main()
