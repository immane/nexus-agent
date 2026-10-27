#!/usr/bin/env python3
"""Launch a cached local SWE-bench task; disconnect task networking before inference."""

import argparse
from datetime import datetime, timezone
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import platform
import re
import secrets
import signal
import socket
import subprocess
import sys
import threading
import time
import uuid
from urllib.parse import urlsplit
from urllib.request import ProxyHandler, Request, build_opener

from run_nexus_swe_task import request, write_json

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
OUTPUT_LOCK = threading.Lock()


def tee_output(source, log, console):
    """Persist chunks before displaying them; a closed terminal must not lose logs."""
    while chunk := source.read1(16384):
        log.write(chunk)
        log.flush()
        if console is not None:
            try:
                with OUTPUT_LOCK:
                    console.write(chunk)
                    console.flush()
            except OSError:
                console = None


def logged_process(argv, env, path):
    """Drain both child streams concurrently with server readiness/runner work."""
    child = subprocess.Popen(argv, cwd=REPO, env=env, stdin=subprocess.DEVNULL,
                             stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    errors = []

    def drain():
        try:
            with child.stdout, path.open("wb") as log:
                tee_output(child.stdout, log, sys.stdout.buffer)
        except OSError as error:
            errors.append(f"{path.name}: {type(error).__name__}")

    thread = threading.Thread(target=drain, daemon=True)
    thread.start()
    return child, thread, errors


def command(args, *, cwd=None, env=None, timeout=120):
    result = subprocess.run([str(arg) for arg in args], cwd=cwd, env=env,
                            capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        # Commands never contain credentials; provider response bodies are not logged.
        raise RuntimeError(f"command failed: {args[0]}: {result.stderr[-2000:].strip()}")
    return result.stdout.strip()


def discover_tasks(root):
    """Read task metadata only, never scan task checkouts or model artifacts."""
    tasks = {}
    for pattern in ("*.task.json", "*/task.json", "*/dataset.json", "*/official-evaluation/dataset.json"):
        for path in sorted(root.glob(pattern)):
            value = json.loads(path.read_text())
            for row in value if isinstance(value, list) else [value]:
                if not isinstance(row, dict) or not all(key in row for key in
                                                       ("instance_id", "base_commit", "repo", "problem_statement")):
                    continue
                key = row["instance_id"]
                if key in tasks and any(tasks[key]["row"][field] != row[field]
                                        for field in ("base_commit", "repo", "problem_statement")):
                    raise RuntimeError(f"conflicting local task metadata: {key}")
                tasks.setdefault(key, {"row": row, "source": str(path)})
    return tasks


def inspect_image(image):
    return json.loads(command(["docker", "image", "inspect", image], timeout=15))[0]


def check_model(provider, model, env):
    """List model identities without generating tokens; bypass workstation proxies."""
    url = provider["endpoint"].rstrip("/") + "/models"
    value = env.get(provider["credential"]["env"])
    headers = {"Authorization": "Bearer " + value} if value else {}
    req = Request(url, headers=headers)
    try:
        with build_opener(ProxyHandler({})).open(req, timeout=8) as response:
            models = json.load(response)
        if not any(entry.get("id") == model["name"] for entry in models.get("data", [])):
            raise RuntimeError("configured wire model is not listed by the model endpoint")
    except (OSError, ValueError) as error:
        raise RuntimeError("model endpoint preflight failed; no inference started") from error


def validate_model_endpoint(provider, model, args, env):
    endpoint = urlsplit(provider.get("endpoint", ""))
    if endpoint.scheme not in ("http", "https") or not endpoint.hostname or endpoint.username or endpoint.password:
        raise RuntimeError("explicit credential-free model endpoint required")
    try:
        address = ipaddress.ip_address(endpoint.hostname)
        local = address.is_private or (address.version == 4 and address in ipaddress.ip_network("100.64.0.0/10"))
    except ValueError:
        local = endpoint.hostname == "localhost"
    if not local and not getattr(args, "allow_hosted_inference", False):
        raise RuntimeError("local benchmark launcher refuses public model endpoints")
    credential = provider["credential"]["env"]
    if not env.get(credential) and provider["id"] != "ollama-local":
        raise RuntimeError(f"missing provider credential environment variable: {credential}")
    check_model(provider, model, env)
    return credential, endpoint.hostname


def available_tasks(root, profiles):
    images = set(command(["docker", "image", "ls", "--format", "{{.Repository}}:{{.Tag}}"], timeout=15).splitlines())
    tasks = discover_tasks(root)
    for instance, task in tasks.items():
        profile = profiles["tasks"].get(instance, {})
        task["image"] = next((image for image in profile.get("images", []) if image in images), None)
        task["exposure"] = profile.get("exposure", "unknown; not established as blind")
    return tasks


def agent_task(row):
    """Do not place gold patches, evaluator scripts, or hidden tests in the agent mount."""
    if not re.fullmatch(r"[A-Za-z0-9_.-]+__[A-Za-z0-9_.-]+-\d+", row["instance_id"]):
        raise RuntimeError("invalid task instance ID")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", row["repo"]):
        raise RuntimeError("invalid dataset repository")
    if not re.fullmatch(r"[a-f0-9]{40}", row["base_commit"]):
        raise RuntimeError("invalid task base commit")
    return {key: row[key] for key in ("instance_id", "repo", "base_commit", "problem_statement", "version") if key in row}


def owner_record(run):
    records = list((run / "state/nexus-agent/container").glob("*.json"))
    if len(records) != 1:
        raise RuntimeError("expected exactly one ownership record for this launcher")
    record = json.loads(records[0].read_text())
    if not re.fullmatch(r"[a-f0-9]{16,64}", record["nonce"]):
        raise RuntimeError("invalid ownership nonce")
    if record["gateway_name"] != "nexus-gw-" + record["nonce"]:
        raise RuntimeError("gateway identity mismatch")
    return record


def verify_owned(record, workspace):
    gateway, workload = record["gateway_id"], record["workload_id"]
    for identity in (gateway, workload):
        if not isinstance(identity, str) or not re.fullmatch(r"[a-f0-9]{64}", identity):
            raise RuntimeError("container preparation incomplete; refusing unverified cleanup")
    containers = json.loads(command(["docker", "inspect", gateway, workload], timeout=15))
    gw, worker = containers
    if (gw["Id"] != gateway or worker["Id"] != workload
            or gw["Config"]["Labels"].get("io.nexus-agent.gateway-owner") != record["gateway_name"]
            or worker["Config"]["Labels"].get("io.nexus-agent.workload-owner") != "workload-" + record["nonce"]
            or worker["HostConfig"]["NetworkMode"] != "container:" + gateway):
        raise RuntimeError("container ownership/network identity mismatch")
    mounts = [mount for mount in worker["Mounts"] if mount["Type"] == "bind"]
    if not any(mount["Destination"] == "/workspace" and Path(mount["Source"]).resolve() == workspace.resolve()
               for mount in mounts):
        raise RuntimeError("workload workspace mount mismatch")
    return gw, worker


def disconnect_network(record, workspace):
    gateway, _ = verify_owned(record, workspace)
    for network in gateway["NetworkSettings"]["Networks"]:
        command(["docker", "network", "disconnect", network, record["gateway_id"]], timeout=15)
    gateway, _ = verify_owned(record, workspace)
    if gateway["NetworkSettings"]["Networks"]:
        raise RuntimeError("gateway still has network attachments")
    # /proc inspection works without task-specific Python/curl dependencies.
    routes = command(["docker", "exec", record["workload_id"], "cat", "/proc/net/route"], timeout=10)
    device_stats = command(["docker", "exec", record["workload_id"], "cat", "/proc/net/dev"], timeout=10)
    # sysfs may reflect the mount's original namespace when Docker uses
    # --network=container; procfs network views reflect the executing process.
    devices = sorted(line.split(":", 1)[0].strip() for line in device_stats.splitlines()[2:] if ":" in line)
    links = command(["docker", "exec", record["gateway_id"], "ip", "-o", "link", "show", "up"], timeout=10)
    up_devices = re.findall(r"^\d+: ([^:@]+)(?:@[^:]+)?:", links, re.MULTILINE)
    # Docker's kernel can expose inactive GRE/IP tunnel devices; only loopback
    # may be UP. They do not provide connectivity after bridge disconnection.
    if up_devices != ["lo"] or len(routes.splitlines()) != 1:
        raise RuntimeError(f"workload still has active interfaces/routes: links={links!r}, routes={routes!r}")
    return {"verified": True, "docker_networks": [], "interfaces": devices,
            "up_interfaces": up_devices, "ipv4_routes": routes}


def cleanup_containers(record, workspace):
    """Only delete exact, label-verified resources of this launch, never global prune."""
    if not re.fullmatch(r"[a-f0-9]{16,64}", record.get("nonce", "")):
        raise RuntimeError("invalid ownership nonce")
    existing = set(command(["docker", "ps", "-aq", "--no-trunc"], timeout=15).splitlines())
    for kind, prefix in (("workload", "workload-"), ("gateway", "nexus-gw-")):
        identity = record.get(kind + "_id")
        if not identity:
            # Runtime quarantine can already have removed the workload and
            # cleared its durable ID. Never infer absence from the ID alone:
            # partial creation can also leave a named container without an ID.
            name = ("nexus-workload-" if kind == "workload" else "nexus-gw-") + record["nonce"]
            if command(["docker", "ps", "-aq", "--no-trunc", "--filter", "name=^" + name + "$"], timeout=15):
                raise RuntimeError("incomplete ownership record; inspect preparation manually")
            continue
        if not re.fullmatch(r"[a-f0-9]{64}", identity):
            raise RuntimeError("invalid container identity")
        if identity not in existing:
            continue
        info = json.loads(command(["docker", "inspect", identity], timeout=15))[0]
        if (info["Id"] != identity or info["Config"]["Labels"].get("io.nexus-agent." + kind + "-owner")
                != prefix + record["nonce"]):
            raise RuntimeError("refusing cleanup of a container with mismatched ownership")
        if kind == "workload" and not any(
                mount["Type"] == "bind" and mount["Destination"] == "/workspace"
                and Path(mount["Source"]).resolve() == workspace.resolve() for mount in info["Mounts"]):
            raise RuntimeError("refusing cleanup of mismatched workspace")
        command(["docker", "rm", "-f", identity], timeout=20)


def verify_worker(record, row):
    """Exercise the versioned file-worker protocol before spending model tokens."""
    # Detached HEAD is launcher-controlled, read-only, and has a known prefix.
    # Non-default paging distinguishes old workers that only accept `path`.
    frame = {"version": 1, "operation": "host_read",
             "arguments": {"path": "/workspace/.git/HEAD", "offset": 1, "limit": 4},
             "output_limit": 4096}
    message = "container file-worker preflight failed; rebuild the task adapter image with current sources; no inference started"
    try:
        result = subprocess.run(
            ["docker", "exec", "--interactive", "--workdir", "/workspace", record["workload_id"],
             "/usr/local/bin/container-worker"],
            input=json.dumps(frame), capture_output=True, text=True, timeout=30)
        if result.returncode:
            raise RuntimeError(message)
        response = json.loads(result.stdout)
        expected = {"version": 1, "result": {
            "status": "Succeeded", "effect": "KnownNotApplied",
            "content": "[more; next_offset=5]\n" + row["base_commit"][1:5], "truncated": True}}
        if response != expected:
            raise RuntimeError(message)
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        raise RuntimeError(message) from error
    return {"verified": True, "protocol_version": 1, "operation": "host_read", "paged_read": True}


def verify_task_environment(record, row):
    """Check checkout authority and imports offline, without exposing evaluator data."""
    modules = {"django/django": "django", "pallets/flask": "flask",
               "pylint-dev/pylint": "pylint", "sphinx-doc/sphinx": "sphinx",
               "astropy/astropy": "astropy", "psf/requests": "requests",
               "pytest-dev/pytest": "pytest", "pydata/xarray": "xarray",
               "scikit-learn/scikit-learn": "sklearn", "matplotlib/matplotlib": "matplotlib"}
    module = modules.get(row["repo"])
    if module is None:
        raise RuntimeError("no import preflight registered for this repository")
    prefix = ["docker", "exec", "-w", "/workspace", record["workload_id"]]
    head = command(prefix + ["git", "rev-parse", "HEAD"], timeout=30)
    if head != row["base_commit"] or command(prefix + ["git", "status", "--porcelain"], timeout=30):
        raise RuntimeError("agent checkout is not clean at the task base")
    support = json.loads(command(prefix + ["python", "/usr/local/lib/nexus-swe-support.py", "restore",
                                          "/opt/nexus-task-support", "/workspace"], timeout=120))
    if command(prefix + ["git", "status", "--porcelain"], timeout=30):
        raise RuntimeError("native build support changed tracked/unignored task files")
    code = ("import importlib, importlib.util, json, pathlib, sys; "
            f"m = importlib.import_module({module!r}); "
            "p = pathlib.Path(m.__file__).resolve(); "
            "assert pathlib.Path('/workspace') in p.parents, str(p); "
            "print(json.dumps({'python': sys.version, 'module': m.__name__, "
            "'source': str(p), 'pytest_available': importlib.util.find_spec('pytest') is not None}))")
    imported = json.loads(command(prefix + ["python", "-c", code], timeout=120))
    return {"base_commit": head, "clean_checkout": True, "native_support": support, "import": imported,
            "public_tests": "not-run; import preflight is not test-suite validation"}


def launch(args, profiles, task):
    if not task["image"] and not args.container_image:
        raise RuntimeError("no cached adapter image; build it first or specify --container-image")
    config = json.loads(args.config.read_text())
    model_id = args.model or profiles["model"]
    provider_id = args.provider or profiles["provider"]
    provider = next((p for p in config["providers"] if p["id"] == provider_id), None)
    model = next((m for m in config["models"] if m["id"] == model_id and m["provider"] == provider_id), None)
    if provider is None or model is None:
        raise RuntimeError("provider/model binding missing from selected config")
    env = os.environ.copy()
    # Probe unauthenticated Ollama before adding the server's compatibility
    # placeholder. Other providers must have their real credential explicitly set.
    credential, endpoint_host = validate_model_endpoint(provider, model, args, env)
    if not env.get(credential):
        env[credential] = "dummy"  # Local Ollama credential gate only; never a real secret.
    bypass = ",".join(filter(None, [env.get("NO_PROXY"), endpoint_host, "127.0.0.1", "localhost"]))
    env.update(NO_PROXY=bypass, no_proxy=bypass)
    row = agent_task(task["row"])
    gateway = inspect_image(args.gateway_image or profiles["gateway_image"])
    workload = inspect_image(args.container_image or task["image"])
    if args.build:
        subprocess.run(["cargo", "build", "--release", "-p", "nexus-server", "--features", "container", "--locked", "--offline"], cwd=REPO, check=True)
    server = REPO / "target/release/nexus-server"
    if not server.is_file():
        raise RuntimeError("server missing; use --build")
    run = args.work_root / ("run-" + args.task + "-" + datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + uuid.uuid4().hex[:8])
    run.mkdir(mode=0o700, parents=True, exist_ok=False)
    server_config = args.config.resolve()
    if args.model_output_tokens is not None:
        # Never rewrite the user's shared model configuration for an experiment.
        model["max_output_tokens"] = args.model_output_tokens
        server_config = run / "model-config.json"
        write_json(server_config, config)
        server_config.chmod(0o600)
    workspace = run / "workspace"
    workspace.mkdir()
    env["XDG_STATE_HOME"] = str(run / "state")
    token = secrets.token_hex(32)
    env["NEXUS_BENCH_LAUNCH_TOKEN"] = token
    write_json(run / "task.json", row)
    revision = command(["git", "rev-parse", "HEAD"], cwd=REPO)
    diff = command(["git", "diff", "HEAD", "--binary"], cwd=REPO)
    manifest = {"status": "preparing", "task": args.task, "task_source": task["source"],
                "exposure": task["exposure"], "os": platform.platform(), "architecture": platform.machine(),
                "python": sys.version, "rustc": command(["rustc", "--version"]),
                "docker": command(["docker", "version", "--format", "{{.Server.Version}}"], timeout=15),
                "nexus_revision": revision, "working_diff_sha256": hashlib.sha256(diff.encode()).hexdigest(),
                "launcher_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                "runner_sha256": hashlib.sha256((HERE / "run_nexus_swe_task.py").read_bytes()).hexdigest(),
                "profile_sha256": hashlib.sha256(args.profiles.read_bytes()).hexdigest(),
                "task_sha256": hashlib.sha256(json.dumps(row, sort_keys=True).encode()).hexdigest(),
                "server_sha256": hashlib.sha256(server.read_bytes()).hexdigest(),
                "gateway_image": gateway["Id"], "workload_image": workload["Id"],
                "workload_architecture": workload["Architecture"], "provider": provider_id, "model": model_id,
                "endpoint": provider["endpoint"], "config_sha256": hashlib.sha256(args.config.read_bytes()).hexdigest(),
                "effective_config_sha256": hashlib.sha256(server_config.read_bytes()).hexdigest(),
                "model_output_tokens": model.get("max_output_tokens"),
                "limits": {"model_turns": 500, "tool_calls": args.max_tool_calls, "run_seconds": args.max_run_seconds,
                           "input_tokens": None, "output_tokens": None}, "evaluation": "not-run"}
    write_json(run / "launch.json", manifest)
    print(f"Artifacts: {run}", flush=True)

    def progress(stage, **detail):
        event = {"at_utc": datetime.now(timezone.utc).isoformat(), "stage": stage, **detail}
        with (run / "launcher-events.jsonl").open("a") as log:
            log.write(json.dumps(event) + "\n")
        print(f"[launcher] {event['at_utc']} {stage} {json.dumps(detail)}", flush=True)

    process = None
    pumps = []
    record = None
    try:
        progress("checkout-started", base_commit=row["base_commit"])
        command(["git", "init", "--quiet", workspace])
        command(["git", "-C", workspace, "fetch", "--quiet", "--depth=1", "https://github.com/" + row["repo"] + ".git", row["base_commit"]])
        command(["git", "-C", workspace, "checkout", "--quiet", "--detach", "FETCH_HEAD"])
        # No refs/remotes or external object alternates; no future commits exposed.
        if command(["git", "-C", workspace, "rev-parse", "HEAD"]) != row["base_commit"]:
            raise RuntimeError("task checkout revision mismatch")
        progress("checkout-ready")
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", args.port))
            port = listener.getsockname()[1]
        url = f"http://127.0.0.1:{port}"
        argv = [str(server), "--port", str(port), "--config", str(server_config),
                "--session-store", str((run / "session-store").resolve()),
                "--tools", "real", "--tools-root", str(workspace.resolve()), "--execution", "container",
                "--gateway-image", gateway["Id"], "--container-image", workload["Id"],
                "--benchmark-pilot", "--auth-token-env", "NEXUS_BENCH_LAUNCH_TOKEN"]
        manifest["server_argv"] = argv
        write_json(run / "launch.json", manifest)
        process, pump, errors = logged_process(argv, env, run / "server.log")
        pumps.append((pump, errors))
        progress("server-started", pid=process.pid, address=url)
        opener = build_opener(ProxyHandler({}))
        for _ in range(100):
            if process.poll() is not None:
                raise RuntimeError("server exited; inspect server.log")
            try:
                with opener.open(url + "/health", timeout=1):
                    break
            except OSError:
                time.sleep(.1)
        else:
            raise RuntimeError("server readiness timed out")
        progress("server-ready")
        session = request(url, token, "/sessions", {"provider": provider_id, "model": model_id})["session"]
        progress("session-created", session=session)
        record = owner_record(run)
        manifest["session"] = session
        manifest["containers"] = record
        manifest["network"] = disconnect_network(record, workspace)
        progress("network-verified", evidence=manifest["network"])
        manifest["worker"] = verify_worker(record, row)
        progress("worker-verified", evidence=manifest["worker"])
        if getattr(args, "verify_task_environment", False):
            manifest["environment"] = verify_task_environment(record, row)
            progress("environment-verified", evidence=manifest["environment"])
        manifest["status"] = "prepared"
        write_json(run / "launch.json", manifest)
        if args.prepare_only:
            print("PASS: session prepared; only loopback is UP, no IPv4 routes; no inference", flush=True)
            return 0
        runner = [sys.executable, "-u", str(HERE / "run_nexus_swe_task.py"),
                  "--task-json", str(run / "task.json"), "--workspace", str(workspace),
                  "--server", url, "--session-id", session, "--token-env", "NEXUS_BENCH_LAUNCH_TOKEN",
                  "--provider", provider_id, "--model", model_id, "--output-dir", str(run / "artifacts"),
                  "--nexus-revision", revision + "+diff-" + manifest["working_diff_sha256"],
                  "--run-label", task["exposure"] + "; network-disabled", "--max-run-seconds", str(args.max_run_seconds),
                  "--max-tool-calls", str(args.max_tool_calls), "--tool-log-bytes", str(args.tool_log_bytes)]
        if profiles.get("context_window_tokens") and model_id == profiles["model"] and provider_id == profiles["provider"]:
            runner += ["--context-window-tokens", str(profiles["context_window_tokens"])]
        manifest["runner_argv"] = runner
        manifest["status"] = "running"
        write_json(run / "launch.json", manifest)
        progress("runner-started")
        child, pump, errors = logged_process(runner, env, run / "model-output.log")
        pumps.append((pump, errors))
        try:
            returncode = child.wait()
        finally:
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=5)
        progress("runner-exited", exit_code=returncode)
        manifest["runner_exit_code"] = returncode
        manifest["status"] = "finished"
        return returncode
    except BaseException as error:
        manifest["status"] = "interrupted-or-failed"
        progress("launch-failed", error_type=type(error).__name__)
        raise
    finally:
        if process is not None:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
        # The server can retain containers after exit. Reconcile only this run's
        # durable ownership record, including a partially successful /sessions call.
        try:
            records = list((run / "state/nexus-agent/container").glob("*.json"))
            if records:
                record = owner_record(run)
                cleanup_containers(record, workspace)
            manifest["cleanup"] = "complete"
            progress("cleanup-complete")
        except (RuntimeError, OSError, subprocess.TimeoutExpired) as error:
            manifest["cleanup"] = "uncertain: " + str(error)
            manifest["status"] = "cleanup-required"
            print(f"Cleanup requires inspection: {run}", file=sys.stderr)
            raise RuntimeError("cleanup uncertain; inspect launch.json and ownership state") from error
        finally:
            log_errors = []
            for pump, errors in pumps:
                pump.join(timeout=5)
                log_errors.extend(errors)
                if pump.is_alive():
                    log_errors.append("output drain incomplete after child shutdown")
            manifest["log_errors"] = log_errors
            if log_errors:
                print(f"WARNING: incomplete output capture: {log_errors}", file=sys.stderr)
            write_json(run / "launch.json", manifest)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("list", "run"))
    parser.add_argument("task", nargs="?")
    parser.add_argument("--work-root", type=Path, default=REPO.parent / "nexus-benchmark-work")
    parser.add_argument("--profiles", type=Path, default=HERE / "local_profiles.json")
    parser.add_argument("--config", type=Path, default=Path.home() / ".config/nexus-agent/config.json")
    parser.add_argument("--provider")
    parser.add_argument("--model")
    parser.add_argument("--model-output-tokens", type=int,
                        help="per-request task output cap; creates a private run-local model config")
    parser.add_argument("--allow-hosted-inference", action="store_true",
                        help="explicitly allow a public model endpoint; inference may incur uncapped provider costs")
    parser.add_argument("--gateway-image")
    parser.add_argument("--container-image")
    parser.add_argument("--port", type=int, default=0, help="0 selects a free loopback port")
    parser.add_argument("--max-run-seconds", type=int, default=86400)
    parser.add_argument("--max-tool-calls", type=int, default=4096)
    parser.add_argument("--tool-log-bytes", type=int, default=8192,
                        help="redacted tool content preview bytes per event; 0 disables content logging")
    parser.add_argument("--prepare-only", action="store_true", help="verify preparation/network and clean up; no inference")
    parser.add_argument("--verify-task-environment", action="store_true",
                        help="verify clean task revision and imports inside the offline workload")
    parser.add_argument("--build", action=argparse.BooleanOptionalAction, default=True)
    args = parser.parse_args()
    if args.max_run_seconds <= 0 or args.max_tool_calls <= 0 or not 0 <= args.port <= 65535:
        parser.error("budgets must be positive and port must be in 0..65535")
    if not 0 <= args.tool_log_bytes <= 65536:
        parser.error("--tool-log-bytes must be in 0..65536")
    if args.model_output_tokens is not None and not 1 <= args.model_output_tokens <= 2**32 - 1:
        parser.error("--model-output-tokens must be in 1..4294967295")
    args.work_root = args.work_root.resolve()
    profiles = json.loads(args.profiles.read_text())
    tasks = available_tasks(args.work_root, profiles)
    if args.action == "list":
        for instance, task in sorted(tasks.items()):
            print(f"{instance}\t{task['image'] or 'NO CACHED ADAPTER'}\t{task['exposure']}")
        return 0
    if args.task not in tasks:
        parser.error("select an installed task with 'list' first")
    return launch(args, profiles, tasks[args.task])


if __name__ == "__main__":
    def interrupted(_signum, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print("benchmark launcher interrupted; partial artifacts preserved", file=sys.stderr)
        raise SystemExit(130)
    except (RuntimeError, OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(f"benchmark launcher: {error}", file=sys.stderr)
        raise SystemExit(2)
