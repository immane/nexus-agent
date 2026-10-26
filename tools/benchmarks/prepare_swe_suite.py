#!/usr/bin/env python3
"""Cache pinned full SWE-bench rows; optionally pull and build adapter images.

The stdlib handles downloads and provenance. Parquet decoding alone uses
`uv run --no-project --with pyarrow==23.0.1` (an isolated, pinned dependency;
requires uv and a Python version supported by that PyArrow release). No model
endpoint is contacted. Keep --work-root outside agent workspaces: its task JSON
and cached Parquet contain gold patches and tests and must never be agent mounts.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
from urllib.request import urlopen

from swebench import HERE, REPO, agent_task, command, inspect_image

PARQUET = "data/test-00000-of-00001.parquet"
PYARROW_VERSION = "23.0.1"
EXPOSURE = "selected diagnostic task; blind status not established"


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def json_bytes(value):
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()


def load_suite(path):
    suite = json.loads(path.read_bytes())
    if suite.get("schema_version") != 1 or not isinstance(suite.get("name"), str):
        raise ValueError("expected named suite schema_version 1")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,63}", suite["name"]):
        raise ValueError("suite name must be a safe directory component")
    dataset = suite["dataset"]
    if (dataset["name"] != "SWE-bench/SWE-bench_Lite" or dataset["split"] != "test"
            or not re.fullmatch(r"[a-f0-9]{40}", dataset["revision"])):
        raise ValueError("expected SWE-bench_Lite test with an immutable dataset commit SHA")
    if suite["platform"] != "linux/amd64" or suite["evaluator_version"] != "5.0.2":
        raise ValueError("expected linux/amd64 and evaluator_version 5.0.2")
    defaults = suite["defaults"]
    for key in ("max_run_seconds", "max_tool_calls", "tool_log_bytes"):
        value = defaults[key]
        if type(value) is not int or value < (0 if key == "tool_log_bytes" else 1):
            raise ValueError(f"invalid suite default: {key}")
    if defaults["tool_log_bytes"] > 65536:
        raise ValueError("tool_log_bytes exceeds launcher limit")
    seen = set()
    if not suite["tasks"]:
        raise ValueError("suite must contain tasks")
    for task in suite["tasks"]:
        instance = task["instance_id"]
        # Reuse launcher's path/repository validation rather than a looser ID parser.
        agent_task({"instance_id": instance, "repo": "owner/repo", "base_commit": "0" * 40,
                    "problem_statement": "validation"})
        if instance in seen:
            raise ValueError("duplicate suite task: " + instance)
        seen.add(instance)
        if task["exposure"] != EXPOSURE or not all(
                isinstance(task[key], str) and task[key].strip() for key in ("domain", "purpose")):
            raise ValueError("task requires domain, purpose and explicit diagnostic exposure")
    return suite


def image_names(instance):
    return ("swebench/sweb.eval.x86_64." + instance.replace("__", "_1776_") + ":latest",
            "nexus-swebench-" + instance + ":diagnostics-v1")


def make_profiles(suite):
    local = json.loads((HERE / "local_profiles.json").read_text())
    profiles = {key: local[key] for key in ("provider", "model", "context_window_tokens", "gateway_image")}
    profiles["tasks"] = {task["instance_id"]: {
        "images": [image_names(task["instance_id"])[1]], "exposure": task["exposure"]
    } for task in suite["tasks"]}
    return profiles


def ensure_json(path, value):
    """Never replace independently prepared or user-modified metadata."""
    if path.exists():
        if json.loads(path.read_bytes()) != value:
            raise RuntimeError(f"conflicting metadata; preserving {path}")
        return
    with path.open("xb") as output:
        output.write(json_bytes(value))
    path.chmod(0o600)


def save_evidence(path, value):
    # Only this preparation's validated evidence is updated, under the root lock.
    fd, temporary = tempfile.mkstemp(prefix=".suite-evidence-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as output:
            output.write(json_bytes(value))
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def download_rows(root, dataset, selected):
    """Read canonical Parquet at a commit URL, verifying its HF LFS SHA-256."""
    base = "https://huggingface.co/"
    api = base + "api/datasets/" + dataset["name"] + "/tree/" + dataset["revision"] + "/data"
    with urlopen(api, timeout=60) as response:
        files = json.load(response)
    entry = next(item for item in files if item["path"] == PARQUET)
    expected = entry["lfs"]["oid"]
    if not re.fullmatch(r"[a-f0-9]{64}", expected):
        raise RuntimeError("dataset file has no valid LFS SHA-256")
    url = base + "datasets/" + dataset["name"] + "/resolve/" + dataset["revision"] + "/" + PARQUET
    cache = root / ".swe-suite" / dataset["revision"]
    cache.mkdir(parents=True, exist_ok=True, mode=0o700)
    parquet = cache / "test.parquet"
    if parquet.exists():
        if sha256(parquet.read_bytes()) != expected:
            raise RuntimeError("conflicting cached Parquet; preserving " + str(parquet))
    else:
        with urlopen(url, timeout=120) as response:
            data = response.read(64 * 1024 * 1024 + 1)
        if sha256(data) != expected:
            raise RuntimeError("downloaded dataset SHA-256 mismatch")
        with parquet.open("xb") as output:
            output.write(data)
        parquet.chmod(0o600)
    decoder = ("import json,sys; import pyarrow.parquet as pq; "
               "ids=set(json.loads(sys.argv[2])); "
               "print(json.dumps([r for r in pq.read_table(sys.argv[1]).to_pylist() "
               "if r['instance_id'] in ids]))")
    rows = json.loads(command(["uv", "run", "--no-project", "--with", "pyarrow==" + PYARROW_VERSION,
                               "python", "-c", decoder, parquet, json.dumps(selected)], timeout=300))
    found = {}
    for row in rows:
        agent_task(row)
        instance = row["instance_id"]
        if instance in found or instance not in selected:
            raise RuntimeError("unexpected or duplicate canonical task row")
        if not all(key in row for key in ("patch", "test_patch", "FAIL_TO_PASS", "PASS_TO_PASS")):
            raise RuntimeError("canonical row is missing evaluator fields")
        found[instance] = row
    if set(found) != set(selected):
        raise RuntimeError("tasks absent from pinned dataset: " + ", ".join(sorted(set(selected) - set(found))))
    return found, {"url": url, "sha256": expected, "file": str(parquet),
                   "decoder": "pyarrow==" + PYARROW_VERSION}


def image_record(info):
    if info["Os"] != "linux" or info["Architecture"] != "amd64":
        raise RuntimeError("image is not linux/amd64")
    if not re.fullmatch(r"sha256:[a-f0-9]{64}", info["Id"]):
        raise RuntimeError("image has no immutable identity")
    return {"id": info["Id"], "repo_digests": info.get("RepoDigests", []), "platform": "linux/amd64"}


def build_source_sha256(root=REPO):
    """Hash the worker Dockerfile's explicit Cargo/crates inputs, including untracked files."""
    paths = [root / name for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml")]
    support = root / "tools/benchmarks/swebench_image_support.py"
    if support.exists():
        paths.append(support)
    for directory, dirs, files in os.walk(root / "crates", followlinks=False):
        dirs[:] = sorted(name for name in dirs if name not in (".git", "target", "secrets"))
        if any((Path(directory) / name).is_symlink() for name in dirs):
            raise RuntimeError("symlinked build directory requires explicit review")
        paths.extend(Path(directory) / name for name in sorted(files)
                     if not (name == ".env" or name.startswith(".env.") and name != ".env.example"))
    digest = hashlib.sha256()
    for path in sorted(paths):
        if path.is_symlink():
            raise RuntimeError("symlinked build input requires explicit review")
        digest.update(str(path.relative_to(root)).encode() + b"\0")
        digest.update(hashlib.sha256(path.read_bytes()).digest())
    digest.update((root / ".dockerignore").read_bytes())
    return digest.hexdigest()


def prepare_image(instance, previous=None, rebuild=False, base_commit=None):
    if not isinstance(base_commit, str) or not re.fullmatch(r"[a-f0-9]{40}", base_commit):
        raise ValueError("adapter build requires the canonical task base commit")
    base, derived = image_names(instance)
    dockerfile_hash = sha256((HERE / "swebench-task-image.Dockerfile").read_bytes())
    source_hash = build_source_sha256()
    if previous:
        print(f"[{instance}] verifying recorded images", flush=True)
        if not rebuild and previous["dockerfile_sha256"] != dockerfile_hash:
            raise RuntimeError("adapter Dockerfile changed since preparation")
        if not rebuild and previous.get("build_source_sha256") != source_hash:
            raise RuntimeError("worker build inputs changed since preparation")
        if not rebuild and previous.get("task_base_commit") != base_commit:
            raise RuntimeError("adapter task base commit changed")
        for name, key in ((base, "base"), (derived, "derived")):
            if image_record(inspect_image(name)) != previous[key]:
                raise RuntimeError("prepared image identity changed: " + name)
        if not rebuild:
            return previous
    existing = command(["docker", "image", "ls", "--quiet", "--filter", "reference=" + derived], timeout=15)
    if existing and previous is None:
        raise RuntimeError("unrecorded adapter image already exists; refusing to overwrite " + derived)
    if previous is None:
        print(f"[{instance}] pulling official linux/amd64 image", flush=True)
        command(["docker", "pull", "--platform", "linux/amd64", base], timeout=3600)
    base_info = image_record(inspect_image(base))
    # BuildKit FROM needs a repository digest, not a local image configuration ID.
    digest = next((value for value in base_info["repo_digests"]
                   if value.startswith(base.rsplit(":", 1)[0] + "@sha256:")), None)
    if not digest:
        raise RuntimeError("pulled official image has no immutable repository digest")
    print(f"[{instance}] building Nexus adapter", flush=True)
    command(["docker", "buildx", "build", "--load", "--platform", "linux/amd64",
             "--build-arg", "TASK_IMAGE=" + digest, "--build-arg", "TASK_BASE_COMMIT=" + base_commit, "-t", derived,
             "-f", HERE / "swebench-task-image.Dockerfile", REPO], timeout=7200)
    if source_hash != build_source_sha256():
        raise RuntimeError("worker source changed during adapter build; image provenance is uncertain")
    return {"base_name": base, "base": base_info, "derived_name": derived,
            "derived": image_record(inspect_image(derived)),
            "dockerfile_sha256": dockerfile_hash, "build_source_sha256": source_hash,
            "task_base_commit": base_commit}


def prepare(args):
    suite = load_suite(args.suite)
    selected = [task["instance_id"] for task in suite["tasks"]
                if not args.task or task["instance_id"] in args.task]
    unknown = set(args.task or []) - set(selected)
    if unknown:
        raise ValueError("unknown suite tasks: " + ", ".join(sorted(unknown)))
    root = args.work_root.resolve()
    if root == REPO or REPO in root.parents:
        raise ValueError("work root must be outside the source repository (contains gold data)")
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    lock = root / ".suite-preparation.lock"
    try:
        fd = os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    except FileExistsError as error:
        raise RuntimeError("preparation lock exists; reconcile interrupted/concurrent preparation first") from error
    os.close(fd)
    try:
        return prepare_locked(args, suite, root, selected)
    finally:
        lock.unlink()


def prepare_locked(args, suite, root, selected):
    path = root / "suite-preparation.json"
    identity = {"schema_version": 1, "suite_name": suite["name"],
                "suite_sha256": sha256(args.suite.read_bytes()), "dataset": suite["dataset"],
                "evaluator_version": suite["evaluator_version"], "platform": suite["platform"]}
    evidence = json.loads(path.read_bytes()) if path.exists() else {**identity, "tasks": {}}
    if any(evidence.get(key) != value for key, value in identity.items()):
        raise RuntimeError("conflicting suite preparation provenance")
    for instance, record in evidence["tasks"].items():
        agent_task({"instance_id": instance, "repo": "owner/repo", "base_commit": "0" * 40,
                    "problem_statement": "validation"})
        if sha256((root / (instance + ".task.json")).read_bytes()) != record["task_sha256"]:
            raise RuntimeError("prepared task metadata changed: " + instance)
    profiles = make_profiles(suite)
    profiles_path = root / "suite-profiles.json"
    if (evidence.get("profiles_sha256") is not None
            and sha256(profiles_path.read_bytes()) != evidence["profiles_sha256"]):
        raise RuntimeError("prepared profiles changed")
    ensure_json(profiles_path, profiles)
    missing = [instance for instance in selected if instance not in evidence["tasks"]]
    if missing:
        print(f"Fetching {len(missing)} canonical rows at {suite['dataset']['revision']}", flush=True)
        rows, source = download_rows(root, suite["dataset"], missing)
        if evidence.get("source") not in (None, source):
            raise RuntimeError("conflicting dataset source provenance")
        evidence["source"] = source
        for instance in missing:
            task_path = root / (instance + ".task.json")
            ensure_json(task_path, rows[instance])
            evidence["tasks"][instance] = {"task_file": str(task_path),
                                            "task_sha256": sha256(task_path.read_bytes())}
            save_evidence(path, evidence)
    if args.images:
        for instance in selected:
            record = evidence["tasks"][instance]
            base_commit = json.loads((root / (instance + ".task.json")).read_bytes())["base_commit"]
            if getattr(args, "rebuild_images", False):
                record["images"] = prepare_image(instance, record.get("images"), rebuild=True, base_commit=base_commit)
            else:
                record["images"] = prepare_image(instance, record.get("images"), base_commit=base_commit)
            save_evidence(path, evidence)
    evidence["profiles_sha256"] = sha256((root / "suite-profiles.json").read_bytes())
    save_evidence(path, evidence)
    print(f"Prepared {len(selected)} tasks; profiles: {root / 'suite-profiles.json'}")
    print(f"Evidence: {path}; images {'verified/built' if args.images else 'not requested'}; no inference")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work-root", type=Path,
                        default=REPO.parent / "nexus-benchmark-work/lite-diagnostics-v1")
    parser.add_argument("--suite", type=Path, default=HERE / "swe_suite.json")
    parser.add_argument("--images", action="store_true", help="explicitly authorize Docker pulls and builds")
    parser.add_argument("--rebuild-images", action="store_true",
                        help="with --images, rebuild only identity-verified recorded adapters after source changes")
    parser.add_argument("--task", action="append", help="select an instance ID; repeatable, manifest order retained")
    args = parser.parse_args(argv)
    if args.rebuild_images and not args.images:
        parser.error("--rebuild-images requires --images")
    return prepare(args)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"suite preparation: {error}", file=sys.stderr)
        raise SystemExit(2)
