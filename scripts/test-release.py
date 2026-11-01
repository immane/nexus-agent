#!/usr/bin/env python3
"""Offline packaging and installation regression tests (no Cargo build)."""

import hashlib
import importlib.util
import json
import platform
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch


def load(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(f"{name}.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


package = load("package-release")
installer = load("install-release")


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "scripts").mkdir()
        (self.root / "binaries").mkdir()
        (self.root / "LICENSE").write_text("test license")
        for name in ("config.sh", "install-release.py"):
            (self.root / "scripts" / name).write_text("test script")
        for name in package.BINARIES:
            (self.root / "binaries" / name).write_text(f"test binary {name}")

    def archive(self, directory="dist"):
        architecture = "aarch64" if platform.machine() == "arm64" else platform.machine()
        target = f"{architecture}-apple-darwin" if platform.system() == "Darwin" else f"{architecture}-unknown-linux-gnu"
        return package.create_archive(self.root, self.root / "binaries",
                                      self.root / directory, "1.0-test", target, {})

    def extract(self):
        archive = self.archive()
        with tarfile.open(archive) as tar:
            tar.extractall(self.root / "extracted", filter="data")
        return next((self.root / "extracted").iterdir())

    def test_deterministic_archive_and_checksum(self):
        first, second = self.archive(), self.archive("second")
        self.assertEqual(first.read_bytes(), second.read_bytes())
        self.assertEqual(first.with_suffix(".gz.sha256").read_text().split()[0],
                         hashlib.sha256(first.read_bytes()).hexdigest())
        with self.assertRaises(FileExistsError):
            self.archive()

    def test_install_and_tamper_rejection_before_writes(self):
        root = self.extract()
        destination = self.root / "installed"
        installer.install(root, destination)
        self.assertEqual(len(list(destination.iterdir())), 4)
        self.assertEqual((destination / "nexus-tui").stat().st_mode & 0o777, 0o755)
        (root / "bin/nexus-headless").write_text("tampered")
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            installer.install(root, self.root / "rejected")
        self.assertFalse((self.root / "rejected").exists())

    def test_source_and_destination_symlinks_rejected(self):
        root = self.extract()
        destination = self.root / "installed"
        destination.mkdir()
        victim = self.root / "victim"
        victim.write_text("unchanged")
        (destination / "nexus-tui").symlink_to(victim)
        with self.assertRaisesRegex(ValueError, "symbolic-link destination"):
            installer.install(root, destination)
        self.assertEqual(victim.read_text(), "unchanged")
        (root / "bin/nexus-tui").unlink()
        (root / "bin/nexus-tui").symlink_to(victim)
        with self.assertRaisesRegex(ValueError, "regular release file"):
            installer.install(root, self.root / "rejected")

    def test_wrong_platform_rejected_before_install(self):
        root = self.extract()
        path = root / "manifest.json"
        manifest = json.loads(path.read_text())
        manifest["machine"] = "different-architecture"
        path.write_text(json.dumps(manifest))
        with self.assertRaisesRegex(ValueError, "platform does not match"):
            installer.install(root, self.root / "rejected")
        self.assertFalse((self.root / "rejected").exists())

    def test_replacement_does_not_modify_existing_hardlink(self):
        root = self.extract()
        destination = self.root / "installed"
        destination.mkdir()
        victim = self.root / "victim"
        victim.write_text("old binary")
        (destination / "nexus-tui").hardlink_to(victim)
        installer.install(root, destination)
        self.assertEqual(victim.read_text(), "old binary")
        self.assertEqual((destination / "nexus-tui").read_text(), "test binary nexus-tui")

    def test_verified_bytes_survive_source_change(self):
        root = self.extract()
        original_mkdir = Path.mkdir

        def mutate_source(path, *args, **kwargs):
            (root / "bin/nexus-tui").write_text("changed after verification")
            return original_mkdir(path, *args, **kwargs)

        with patch.object(Path, "mkdir", mutate_source):
            installer.install(root, self.root / "installed")
        self.assertEqual((self.root / "installed/nexus-tui").read_text(), "test binary nexus-tui")

    def test_dangling_checksum_symlink_does_not_get_followed(self):
        destination = self.root / "dist"
        destination.mkdir()
        archive = self.archive("initial")
        sidecar = destination / f"{archive.name}.sha256"
        victim = self.root / "missing"
        sidecar.symlink_to(victim)
        with self.assertRaises(FileExistsError):
            self.archive()
        self.assertFalse(victim.exists())

    def test_rust_python_architecture_mismatch_rejected(self):
        with patch.object(package.platform, "system", return_value="Darwin"), \
                patch.object(package.platform, "machine", return_value="x86_64"):
            with self.assertRaisesRegex(ValueError, "matching native Python"):
                package.create_archive(self.root, self.root / "binaries", self.root / "rejected",
                                       "1.0-test", "aarch64-apple-darwin", {})
            with self.assertRaisesRegex(ValueError, "matching native Python"):
                package.native_platform("aarch64-apple-darwin")
        self.assertFalse((self.root / "rejected").exists())

    def test_target_platform_mapping(self):
        for target, system, machine in (
            ("aarch64-apple-darwin", "Darwin", "arm64"),
            ("x86_64-apple-darwin", "Darwin", "x86_64"),
            ("aarch64-unknown-linux-gnu", "Linux", "aarch64"),
            ("x86_64-unknown-linux-gnu", "Linux", "x86_64"),
        ):
            with self.subTest(target=target), \
                    patch.object(package.platform, "system", return_value=system), \
                    patch.object(package.platform, "machine", return_value=machine):
                self.assertEqual(package.native_platform(target), (system, machine))


if __name__ == "__main__":
    unittest.main()
