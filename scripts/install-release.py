#!/usr/bin/env python3
"""Verify an extracted Nexus release and install its four commands locally."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import tempfile


def install(root, destination):
    manifest = json.loads((root / "manifest.json").read_text())
    if manifest["schema"] != "nexus.release/1":
        raise ValueError("unsupported release manifest")
    if (manifest["system"], manifest["machine"]) != (platform.system(), platform.machine()):
        raise ValueError("release platform does not match this host")
    names = ("nexus-tui", "nexus-server", "nexus-headless", "nexus-config")
    verified = {}
    # Verify every installed file before modifying the destination. Checksums
    # detect corruption, not authenticity: obtain the archive from a trusted source.
    for name in names:
        source = root / "bin" / name
        if source.is_symlink() or not source.is_file():
            raise ValueError(f"not a regular release file: {name}")
        metadata = manifest["files"][f"bin/{name}"]
        data = source.read_bytes()
        if len(data) != metadata["bytes"] or hashlib.sha256(data).hexdigest() != metadata["sha256"]:
            raise ValueError(f"release checksum mismatch: {name}")
        verified[name] = data
    destination.mkdir(parents=True, exist_ok=True)
    for name in names:
        target = destination / name
        if target.is_symlink():
            raise ValueError(f"refusing symbolic-link destination: {target}")
    for name in names:
        target = destination / name
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(dir=destination, prefix=f".{name}-", delete=False) as file:
                temporary = Path(file.name)
                file.write(verified[name])
                os.fchmod(file.fileno(), 0o755)
            os.replace(temporary, target)
        finally:
            if temporary is not None:
                temporary.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", nargs="?", type=Path, default=Path.home() / ".local/bin")
    args = parser.parse_args()
    install(Path(__file__).resolve().parent, args.destination.expanduser())
    print(f"Installed Nexus commands to {args.destination}; add this directory to PATH.")


if __name__ == "__main__":
    main()
