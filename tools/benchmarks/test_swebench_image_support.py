"""Native build support must not carry Git history or overwrite agent source."""

import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import swebench_image_support as support


class SupportTests(unittest.TestCase):
    def setUp(self):
        tree = patch.object(support, "tree_contents", return_value={})
        check = patch.object(support.subprocess, "check_call")
        tree.start()
        check.start()
        self.addCleanup(tree.stop)
        self.addCleanup(check.stop)

    def test_revision_uses_python36_compatible_text_decoding(self):
        with patch.object(support.subprocess, "check_output", return_value="a" * 40 + "\n") as command:
            self.assertEqual(support.revision(Path("workspace")), "a" * 40)
        command.assert_called_once_with(
            ["git", "-C", "workspace", "rev-parse", "HEAD"], universal_newlines=True
        )

    def test_capture_only_build_products_and_restore_same_revision(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, cache, destination = root / "source", root / "support", root / "workspace"
            (source / "package").mkdir(parents=True)
            (source / ".git").mkdir()
            (source / "package/native.so").write_bytes(b"native")
            (source / "package/future_fix.py").write_text("not build support")
            (source / ".git/config").write_text("private Git history")
            destination.mkdir()
            with patch.object(support, "revision", return_value="a" * 40):
                support.capture(source, cache)
                support.restore(cache, destination)
            self.assertEqual((destination / "package/native.so").read_bytes(), b"native")
            self.assertFalse((destination / "package/future_fix.py").exists())
            self.assertFalse((destination / ".git").exists())
            manifest = json.loads((cache / "manifest.json").read_text())
            self.assertEqual(list(manifest["files"]), ["package/native.so"])
            with patch.object(support, "revision", return_value="b" * 40), \
                    self.assertRaisesRegex(RuntimeError, "base commit"):
                support.restore(cache, destination)
            (destination / "package/native.so").write_bytes(b"different")
            with patch.object(support, "revision", return_value="a" * 40), \
                    self.assertRaisesRegex(RuntimeError, "overwrite"):
                support.restore(cache, destination)

    def test_corrupted_support_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, cache, destination = root / "source", root / "support", root / "workspace"
            source.mkdir()
            destination.mkdir()
            (source / "native.so").write_bytes(b"native")
            with patch.object(support, "revision", return_value="a" * 40):
                support.capture(source, cache)
                (cache / "files/native.so").write_bytes(b"corrupt")
                with self.assertRaisesRegex(RuntimeError, "content changed"):
                    support.restore(cache, destination)

    def test_pytest_generated_version_is_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, cache, destination = root / "source", root / "support", root / "workspace"
            version = Path("src/_pytest/_version.py")
            (source / version).parent.mkdir(parents=True)
            (source / version).write_text("version = '5.4.2'\n")
            destination.mkdir()
            with patch.object(support, "revision", return_value="a" * 40):
                support.capture(source, cache)
                support.restore(cache, destination)
            self.assertEqual((destination / version).read_text(), (source / version).read_text())

    def test_setup_commit_is_allowed_only_when_base_contents_match(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with patch.object(support, "revision", return_value="b" * 40):
                support.capture(root, root / "cache", "a" * 40)
            manifest = json.loads((root / "cache/manifest.json").read_text())
            self.assertEqual(manifest["base_commit"], "a" * 40)
            self.assertEqual(manifest["image_head"], "b" * 40)
            with patch.object(support, "revision", return_value="b" * 40), \
                    patch.object(support, "tree_contents", side_effect=[{"path": "base"}, {"path": "fix"}]), \
                    self.assertRaisesRegex(RuntimeError, "differs"):
                support.capture(root, root / "bad-cache", "a" * 40)


if __name__ == "__main__":
    unittest.main()
