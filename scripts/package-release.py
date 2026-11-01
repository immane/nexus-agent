#!/usr/bin/env python3
"""Build a native, installer-aligned release archive without publishing it."""

import argparse
import gzip
import hashlib
import io
import json
from pathlib import Path
import platform
import subprocess
import tarfile
import tomllib


BINARIES = ("nexus-tui", "nexus-server", "nexus-headless")
FEATURES = "nexus-tui/container,nexus-server/container"


def output(argv, root):
    return subprocess.check_output(argv, cwd=root, text=True).strip()


def native_platform(target):
    architecture = target.split("-", 1)[0]
    if architecture not in ("aarch64", "x86_64"):
        raise ValueError(f"unsupported release architecture: {target}")
    if target.endswith("-apple-darwin"):
        system, machine = "Darwin", "arm64" if architecture == "aarch64" else architecture
    elif "-linux-" in target:
        system, machine = "Linux", architecture
    else:
        raise ValueError(f"unsupported release target: {target}")
    if (system, machine) != (platform.system(), platform.machine()):
        raise ValueError(f"Rust target {target} does not match the Python host platform; "
                         "use matching native Python and Rust toolchains")
    return system, machine


def create_archive(root, binary_dir, destination, version, target, provenance):
    system, machine = native_platform(target)
    name = f"nexus-agent-{version}-{target}"
    destination.mkdir(parents=True, exist_ok=True)
    archive = destination / f"{name}.tar.gz"
    sidecar_path = archive.with_suffix(archive.suffix + ".sha256")
    if archive.exists() or sidecar_path.exists() or sidecar_path.is_symlink():
        raise FileExistsError(f"release output already exists: {archive}")
    files = {f"bin/{binary}": ((binary_dir / binary).read_bytes(), 0o755)
             for binary in BINARIES}
    files.update({
        "bin/nexus-config": ((root / "scripts/config.sh").read_bytes(), 0o755),
        "install.py": ((root / "scripts/install-release.py").read_bytes(), 0o755),
        "LICENSE": ((root / "LICENSE").read_bytes(), 0o644),
    })
    manifest = {
        "schema": "nexus.release/1", "version": version, "target": target,
        "system": system, "machine": machine,
        "build": provenance,
        "files": {path: {"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest(),
                         "mode": format(mode, "04o")}
                  for path, (data, mode) in files.items()},
    }
    files["manifest.json"] = ((json.dumps(manifest, indent=2) + "\n").encode(), 0o644)
    # Fixed gzip/tar metadata makes packaging identical for identical inputs;
    # it does not claim that the compiler produces reproducible binaries.
    with archive.open("xb") as raw:
        with gzip.GzipFile(fileobj=raw, mode="wb", filename="", mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w") as tar:
                for path, (data, mode) in sorted(files.items()):
                    entry = tarfile.TarInfo(f"{name}/{path}")
                    entry.size, entry.mode = len(data), mode
                    tar.addfile(entry, io.BytesIO(data))
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    with archive.with_suffix(archive.suffix + ".sha256").open("x") as sidecar:
        sidecar.write(f"{digest}  {archive.name}\n")
    return archive


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, default=Path("dist"))
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    if platform.system() not in ("Darwin", "Linux"):
        parser.error("only native Linux and macOS builds are supported")
    compiler = output(["rustc", "-vV"], root)
    target = next(line.removeprefix("host: ") for line in compiler.splitlines()
                  if line.startswith("host: "))
    native_platform(target)
    version = tomllib.loads((root / "crates/nexus-tui/Cargo.toml").read_text())["package"]["version"]
    commit = output(["git", "rev-parse", "HEAD"], root)
    tracked_diff = subprocess.check_output(["git", "diff", "HEAD", "--binary"], cwd=root)
    status = output(["git", "status", "--porcelain", "--untracked-files=all"], root)
    source_paths = set(subprocess.check_output(["git", "ls-files", "-z"], cwd=root).decode().split("\0"))
    source_paths.update(f"scripts/{name}.py" for name in
                        ("package-release", "install-release", "test-release"))
    source_hash = hashlib.sha256()
    for path in sorted(source_paths - {""}):
        file = root / path
        if file.is_file():
            source_hash.update(path.encode() + b"\0" + hashlib.sha256(file.read_bytes()).digest())
    provenance = {
        "source_commit": commit, "source_dirty": bool(status),
        "source_files_sha256": source_hash.hexdigest(),
        "source_files_scope": "existing tracked files plus the three release scripts; excludes other untracked files",
        "tracked_diff_sha256": hashlib.sha256(tracked_diff).hexdigest(),
        "untracked_files_present": any(line.startswith("?? ") for line in status.splitlines()),
        "rustc": compiler, "cargo": output(["cargo", "--version"], root),
        "lockfile_sha256": hashlib.sha256((root / "Cargo.lock").read_bytes()).hexdigest(),
        "profile": "release", "features": FEATURES.split(","), "locked": True,
        "offline": args.offline,
    }
    target_dir = root / "target/release-package"
    command = ["cargo", "build", "--release", "--locked", "--target", target,
               "--target-dir", str(target_dir)]
    for binary in BINARIES:
        command.extend(["-p", binary])
    command.extend(["--features", FEATURES, "--bins"])
    if args.offline:
        command.append("--offline")
    subprocess.run(command, cwd=root, check=True)
    archive = create_archive(root, target_dir / target / "release", args.output_dir.resolve(),
                             version, target, provenance)
    print(f"{archive} ({archive.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
