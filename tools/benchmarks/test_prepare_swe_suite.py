"""Offline preparation contracts; never download, invoke uv, or contact Docker."""

import copy
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import prepare_swe_suite as prepare


def row(instance):
    return {"instance_id": instance, "repo": "owner/repo", "base_commit": "a" * 40,
            "problem_statement": "Fix behavior", "version": "1.0", "patch": "gold patch",
            "test_patch": "gold test", "FAIL_TO_PASS": ["test_fix"], "PASS_TO_PASS": [],
            "extra_canonical_field": "preserved"}


class PreparationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.suite = prepare.load_suite(prepare.HERE / "swe_suite.json")
        self.ids = [task["instance_id"] for task in self.suite["tasks"]]
        self.source = {"url": "https://example.invalid/pinned.parquet", "sha256": "b" * 64}

    def invoke(self, *extra):
        return prepare.main(["--work-root", str(self.root), *extra])

    def fake_download(self, _root, _dataset, selected):
        return {instance: row(instance) for instance in selected}, self.source

    def test_manifest_contract_and_profiles(self):
        self.assertEqual(self.ids, ["pallets__flask-4992", "pylint-dev__pylint-6506",
                         "sphinx-doc__sphinx-8595", "django__django-12700", "astropy__astropy-14995",
                         "psf__requests-1963", "pytest-dev__pytest-7373", "pydata__xarray-4094",
                         "scikit-learn__scikit-learn-25570", "matplotlib__matplotlib-23299"])
        self.assertEqual(self.suite["dataset"]["revision"], "b0dde1093fe417d83b7184254edf8199c1f0dff5")
        self.assertEqual(self.suite["defaults"], {"max_run_seconds": 86400, "max_tool_calls": 4096,
                                                 "tool_log_bytes": 8192})
        profiles = prepare.make_profiles(self.suite)
        local = json.loads((prepare.HERE / "local_profiles.json").read_text())
        for key in ("provider", "model", "context_window_tokens", "gateway_image"):
            self.assertEqual(profiles[key], local[key])
        self.assertEqual(list(profiles["tasks"]), self.ids)
        self.assertEqual(prepare.image_names(self.ids[0]), (
            "swebench/sweb.eval.x86_64.pallets_1776_flask-4992:latest",
            "nexus-swebench-pallets__flask-4992:diagnostics-v1"))

    def test_metadata_only_full_rows_and_idempotent_rerun(self):
        with patch.object(prepare, "download_rows", side_effect=self.fake_download) as download, \
                patch.object(prepare, "command", side_effect=AssertionError("unexpected command")), \
                patch.object(prepare, "prepare_image", side_effect=AssertionError("unexpected Docker")):
            self.assertEqual(self.invoke(), 0)
            before = (self.root / "suite-preparation.json").read_bytes()
            self.assertEqual(self.invoke(), 0)
            self.assertEqual(download.call_count, 1)
            self.assertEqual(before, (self.root / "suite-preparation.json").read_bytes())
        evidence = json.loads(before)
        self.assertEqual(evidence["suite_sha256"], prepare.sha256((prepare.HERE / "swe_suite.json").read_bytes()))
        for instance in self.ids:
            path = self.root / (instance + ".task.json")
            self.assertEqual(json.loads(path.read_bytes()), row(instance))
            self.assertEqual(evidence["tasks"][instance]["task_sha256"], prepare.sha256(path.read_bytes()))
            self.assertNotIn("images", evidence["tasks"][instance])

    def test_repeated_selection_retains_order_and_extends_evidence(self):
        with patch.object(prepare, "download_rows", side_effect=self.fake_download) as download:
            self.invoke("--task", self.ids[2], "--task", self.ids[0], "--task", self.ids[0])
            self.assertEqual(download.call_args.args[2], [self.ids[0], self.ids[2]])
            self.invoke("--task", self.ids[1])
        evidence = json.loads((self.root / "suite-preparation.json").read_bytes())
        self.assertEqual(set(evidence["tasks"]), set(self.ids[:3]))
        profiles = json.loads((self.root / "suite-profiles.json").read_bytes())
        self.assertEqual(set(profiles["tasks"]), set(self.ids))

    def test_conflicting_existing_row_preserved(self):
        path = self.root / (self.ids[0] + ".task.json")
        path.write_text('{"existing": true}')
        with patch.object(prepare, "download_rows", side_effect=self.fake_download):
            with self.assertRaisesRegex(RuntimeError, "conflicting metadata"):
                self.invoke()
        self.assertEqual(path.read_text(), '{"existing": true}')
        self.assertFalse((self.root / ".suite-preparation.lock").exists())

    def test_tampered_row_profiles_and_manifest_rejected(self):
        with patch.object(prepare, "download_rows", side_effect=self.fake_download):
            self.invoke()
        path = self.root / (self.ids[0] + ".task.json")
        original = path.read_bytes()
        path.write_text("{}")
        with self.assertRaisesRegex(RuntimeError, "task metadata changed"):
            self.invoke()
        path.write_bytes(original)
        profiles = self.root / "suite-profiles.json"
        original_profiles = profiles.read_bytes()
        profiles.write_text("{}")
        with self.assertRaisesRegex(RuntimeError, "profiles changed"):
            self.invoke()
        profiles.write_bytes(original_profiles)
        suite_path = self.root / "other-suite.json"
        modified = copy.deepcopy(self.suite)
        modified["name"] = "different"
        suite_path.write_bytes(prepare.json_bytes(modified))
        with self.assertRaisesRegex(RuntimeError, "conflicting suite"):
            self.invoke("--suite", str(suite_path))

    def test_unknown_id_and_existing_lock_fail_without_work(self):
        with patch.object(prepare, "download_rows", side_effect=AssertionError("download")):
            with self.assertRaisesRegex(ValueError, "unknown suite tasks"):
                self.invoke("--task", "../bad")
            lock = self.root / ".suite-preparation.lock"
            lock.touch()
            with self.assertRaisesRegex(RuntimeError, "lock exists"):
                self.invoke()
            self.assertTrue(lock.exists())

    def test_invalid_manifest_rejected(self):
        for mutation in (lambda s: s["dataset"].update(revision="main"),
                         lambda s: s["tasks"].append(s["tasks"][0]),
                         lambda s: s["tasks"][0].update(instance_id="../bad"),
                         lambda s: s["defaults"].update(max_run_seconds=True),
                         lambda s: s["tasks"][0].update(exposure="blind")):
            suite = copy.deepcopy(self.suite)
            mutation(suite)
            path = self.root / "invalid.json"
            path.write_bytes(prepare.json_bytes(suite))
            with self.assertRaises((ValueError, RuntimeError)):
                prepare.load_suite(path)

    def test_images_only_when_explicit_and_evidence_kept(self):
        image = {"base": {"id": "sha256:" + "c" * 64}, "derived": {"id": "sha256:" + "d" * 64}}
        with patch.object(prepare, "download_rows", side_effect=self.fake_download), \
                patch.object(prepare, "prepare_image", return_value=image) as build:
            self.invoke("--task", self.ids[0])
            build.assert_not_called()
            self.invoke("--task", self.ids[0], "--images")
            build.assert_called_once_with(self.ids[0], None, base_commit="a" * 40)
            self.invoke("--task", self.ids[0], "--images")
            self.assertEqual(build.call_args.args, (self.ids[0], image))
        evidence = json.loads((self.root / "suite-preparation.json").read_bytes())
        self.assertEqual(evidence["tasks"][self.ids[0]]["images"], image)

    def test_image_build_uses_digest_and_explicit_platform(self):
        base, derived = prepare.image_names(self.ids[0])
        digest = base.rsplit(":", 1)[0] + "@sha256:" + "a" * 64
        info = {"Id": "sha256:" + "b" * 64, "Os": "linux", "Architecture": "amd64",
                "RepoDigests": [digest]}
        with patch.object(prepare, "command", return_value="") as command, \
                patch.object(prepare, "inspect_image", return_value=info):
            record = prepare.prepare_image(self.ids[0], base_commit="a" * 40)
        calls = [call.args[0] for call in command.call_args_list]
        self.assertEqual(calls[1], ["docker", "pull", "--platform", "linux/amd64", base])
        self.assertIn("TASK_IMAGE=" + digest, calls[2])
        self.assertIn("--load", calls[2])
        self.assertEqual(calls[2][-1], prepare.REPO)
        self.assertEqual(record["derived_name"], derived)
        with patch.object(prepare, "command", return_value="existing"):
            with self.assertRaisesRegex(RuntimeError, "refusing to overwrite"):
                prepare.prepare_image(self.ids[0], base_commit="a" * 40)
        info["Architecture"] = "arm64"
        with self.assertRaisesRegex(RuntimeError, "linux/amd64"):
            prepare.image_record(info)

    def test_source_fingerprint_includes_untracked_crate_inputs(self):
        root = self.root / "context"
        (root / "crates/example/src").mkdir(parents=True)
        for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".dockerignore"):
            (root / name).write_text("fixture")
        initial = prepare.build_source_sha256(root)
        source = root / "crates/example/src/untracked.rs"
        source.write_text("fn helper() {}")
        self.assertNotEqual(initial, prepare.build_source_sha256(root))
        stable = prepare.build_source_sha256(root)
        (root / "crates/example/target").mkdir()
        (root / "crates/example/target/cache").write_text("not in Docker context")
        self.assertEqual(stable, prepare.build_source_sha256(root))

    def test_pinned_download_checksum_full_rows_and_cache(self):
        data = b"fake parquet decoded only by mocked subprocess"
        metadata = [{"path": prepare.PARQUET, "lfs": {"oid": prepare.sha256(data)}}]
        responses = [io.BytesIO(json.dumps(metadata).encode()), io.BytesIO(data),
                     io.BytesIO(json.dumps(metadata).encode())]
        with patch.object(prepare, "urlopen", side_effect=responses) as download, \
                patch.object(prepare, "command", return_value=json.dumps([row(self.ids[0])])) as decode:
            rows, source = prepare.download_rows(self.root, self.suite["dataset"], [self.ids[0]])
            self.assertEqual(rows[self.ids[0]], row(self.ids[0]))
            self.assertIn("/resolve/" + self.suite["dataset"]["revision"] + "/", source["url"])
            self.assertEqual(source["sha256"], prepare.sha256(data))
            self.assertEqual(decode.call_args.args[0][:5],
                             ["uv", "run", "--no-project", "--with", "pyarrow==23.0.1"])
            prepare.download_rows(self.root, self.suite["dataset"], [self.ids[0]])
            self.assertEqual(download.call_count, 3)  # second call reuses verified Parquet

    def test_download_fails_closed_on_checksum_or_missing_rows(self):
        data = b"fake parquet"
        metadata = [{"path": prepare.PARQUET, "lfs": {"oid": "0" * 64}}]
        with patch.object(prepare, "urlopen", side_effect=[
                io.BytesIO(json.dumps(metadata).encode()), io.BytesIO(data)]), \
                patch.object(prepare, "command") as decode:
            with self.assertRaisesRegex(RuntimeError, "SHA-256 mismatch"):
                prepare.download_rows(self.root, self.suite["dataset"], [self.ids[0]])
            decode.assert_not_called()
        metadata[0]["lfs"]["oid"] = prepare.sha256(data)
        with patch.object(prepare, "urlopen", side_effect=[
                io.BytesIO(json.dumps(metadata).encode()), io.BytesIO(data)]), \
                patch.object(prepare, "command", return_value="[]"):
            with self.assertRaisesRegex(RuntimeError, "tasks absent"):
                prepare.download_rows(self.root, self.suite["dataset"], [self.ids[0]])
        for result in ([row(self.ids[0]), row(self.ids[0])],
                       [{k: v for k, v in row(self.ids[0]).items() if k != "patch"}]):
            with patch.object(prepare, "urlopen", return_value=io.BytesIO(json.dumps(metadata).encode())), \
                    patch.object(prepare, "command", return_value=json.dumps(result)):
                with self.assertRaises(RuntimeError):
                    prepare.download_rows(self.root, self.suite["dataset"], [self.ids[0]])


if __name__ == "__main__":
    unittest.main()
