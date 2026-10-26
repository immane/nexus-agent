#!/usr/bin/env python3
"""Preserve base-revision build products; compatible with task Python 3.6+."""

import hashlib
import json
import re
from pathlib import Path
import shutil
import subprocess
import sys


GENERATED = ("astropy/version.py", "astropy/_version.py", "src/_pytest/_version.py", "lib/matplotlib/_version.py",
             "lib/matplotlib/mpl-data/matplotlibrc")


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def revision(root):
    return subprocess.check_output(
        ["git", "-C", str(root), "rev-parse", "HEAD"], universal_newlines=True
    ).strip()


def tree_contents(root, commit):
    tree = subprocess.check_output(["git", "-C", str(root), "ls-tree", "-rz", commit])
    contents = {}
    for entry in tree.split(b"\0"):
        if not entry:
            continue
        header, path = entry.split(b"\t", 1)
        mode, kind, identity = header.split()
        # Official images can add a setup commit changing only executable bits.
        # Ignore that distinction, but never ignore blob, link, or path changes.
        contents[path] = (b"file" if mode in (b"100644", b"100755") else mode, kind, identity)
    return contents


def capture(root, support, base_commit=None):
    image_head = revision(root)
    if base_commit is None:
        base_commit = image_head
    if not re.fullmatch(r"[a-f0-9]{40}", base_commit):
        raise RuntimeError("invalid task base commit")
    if tree_contents(root, base_commit) != tree_contents(root, "HEAD"):
        raise RuntimeError("official image source differs from task base contents")
    subprocess.check_call(["git", "-C", str(root), "-c", "core.fileMode=false", "diff", "--quiet", "HEAD"])
    support.mkdir(parents=True, exist_ok=False)
    manifest = {"base_commit": base_commit, "image_head": image_head, "files": {}}
    paths = list(root.rglob("*.so")) + [root / name for name in GENERATED]
    for source in sorted(set(paths)):
        relative = source.relative_to(root)
        if (not source.is_file() or source.is_symlink()
                or any(part in (".git", "build", ".tox", ".venv") for part in relative.parts)):
            continue
        target = support / "files" / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)
        manifest["files"][relative.as_posix()] = digest(target)
    (support / "manifest.json").write_text(json.dumps(manifest, sort_keys=True))


def restore(support, root):
    manifest = json.loads((support / "manifest.json").read_text())
    if revision(root) != manifest["base_commit"]:
        raise RuntimeError("native support does not match task base commit")
    for name, expected in manifest["files"].items():
        relative = Path(name)
        if relative.is_absolute() or ".." in relative.parts:
            raise RuntimeError("invalid native support path")
        source, target = support / "files" / relative, root / relative
        if source.is_symlink() or digest(source) != expected:
            raise RuntimeError("native support content changed")
        if root.resolve() not in target.resolve().parents or target.is_symlink():
            raise RuntimeError("native support target escapes workspace")
        if target.exists():
            if digest(target) != expected:
                raise RuntimeError("refusing to overwrite different workspace file")
        else:
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target)
    print(json.dumps({"base_commit": manifest["base_commit"], "restored_files": len(manifest["files"])}))


if __name__ == "__main__":
    action, first, second = sys.argv[1:4]
    if action == "capture":
        capture(Path(first), Path(second), sys.argv[4] if len(sys.argv) == 5 else None)
    elif action == "restore":
        restore(Path(first), Path(second))
    else:
        raise SystemExit("expected capture or restore")
