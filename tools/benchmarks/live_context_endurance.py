#!/usr/bin/env python3
"""Opt-in live regression for long-lived sessions against real providers.

This is not part of the default test suite. It starts a real nexus-server,
asks the configured model to run enough archived commands to exceed the local
1 MiB context budget, restarts the server, restores the same saved session, and
verifies that the session continues without a local-context LimitReached.

The script reads the default user configuration and stored credentials; it
never prints either. The model provider and credentials are therefore exactly
the ones already configured on this machine.
"""

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import secrets
import signal
import socket
import subprocess
import sys
import time
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

SCHEMA = "nexus.live-context-endurance/1"
LOCAL_CONTEXT_BYTES = 1_048_576
WAIT_REPORT_SECONDS = 30


def write_json(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def api(base, token, path, body=None, timeout=30):
    data = None if body is None else json.dumps(body).encode()
    headers = {"Authorization": f"Bearer {token}"}
    if data is not None:
        headers["Content-Type"] = "application/json"
    request = Request(base + path, data=data, headers=headers)
    try:
        with urlopen(request, timeout=timeout) as response:
            return json.loads(response.read())
    except HTTPError as error:
        # Keep the server's actionable message; it never contains credentials.
        detail = error.read().decode("utf-8", errors="replace")[:500]
        raise RuntimeError(f"server returned HTTP {error.code} for {path}: {detail}") from None


def wait_ready(base, token, process, timeout=60):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"nexus-server exited with status {process.returncode}")
        try:
            api(base, token, "/sessions", timeout=5)
            return
        except (HTTPError, URLError, TimeoutError, ConnectionError, json.JSONDecodeError):
            time.sleep(0.25)
    raise RuntimeError("nexus-server did not become ready")


def start_server(binary, port, token_env, root, store, log_path, extra=()):
    environment = os.environ.copy()
    environment[token_env] = secrets.token_hex(32)
    with log_path.open("ab") as log:
        process = subprocess.Popen(
            [
                str(binary),
                "--port", str(port),
                "--auth-token-env", token_env,
                "--tools", "real",
                "--tools-root", str(root),
                "--session-store", str(store),
                "--execution", "local",
                *extra,
            ],
            stdout=log,
            stderr=subprocess.STDOUT,
            env=environment,
        )
    return process, environment[token_env]


def stop_server(process, timeout=30):
    if process.poll() is not None:
        return
    process.send_signal(signal.SIGTERM)
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=timeout)


def stream_run(base, token, session, run, timeout):
    """Reads one run's SSE stream and returns its terminal event."""
    path = f"/sessions/{session}/runs/{run}/events"
    request = Request(base + path, headers={"Authorization": f"Bearer {token}"})
    deadline = time.monotonic() + timeout
    last_report = time.monotonic()
    events = 0
    with urlopen(request, timeout=timeout) as response:
        for raw in response:
            if time.monotonic() > deadline:
                raise RuntimeError("live run exceeded its wall-clock timeout")
            line = raw.decode("utf-8", errors="replace").strip()
            if not line.startswith("data: "):
                if time.monotonic() - last_report >= WAIT_REPORT_SECONDS:
                    print(f"[wait] events={events} elapsed={int(time.monotonic() - (deadline - timeout))}s", file=sys.stderr, flush=True)
                    last_report = time.monotonic()
                continue
            event = json.loads(line[6:])
            events += 1
            if event.get("terminal"):
                return event
    raise RuntimeError("event stream closed before a terminal event")


def submit_and_wait(base, token, session, prompt, timeout, report):
    reply = api(base, token, f"/sessions/{session}/runs", {
        "input": prompt,
        "delivery": "queue",
        "submission_id": secrets.token_hex(16),
    })
    if reply.get("reply") != "accepted" or not isinstance(reply.get("run"), str):
        raise RuntimeError(f"server did not accept the prompt: {reply}")
    run = reply["run"]
    started = time.monotonic()
    terminal = stream_run(base, token, session, run, timeout)
    detail = terminal.get("detail") or {}
    outcome = detail.get("outcome")
    snapshot = api(base, token, f"/sessions/{session}/snapshot?run={run}")
    report["runs"].append({
        "run": run,
        "outcome": outcome,
        "snapshot_outcome": snapshot.get("outcome"),
        "elapsed_seconds": round(time.monotonic() - started, 3),
        "events": terminal.get("seq"),
        "error": detail.get("error"),
    })
    return outcome, snapshot


def diagnostic_lines(text):
    for line in text.splitlines():
        if line.startswith("nexus-runtime:") or line.startswith("nexus-server:"):
            yield line


def parse_diagnostics(text):
    result = {
        "reductions": [],
        "exhausted": [],
        "budgets": [],
        "finished": [],
    }
    for line in diagnostic_lines(text):
        match = re.search(
            r"context reduced: removed_items=(\d+) retained_items=(\d+) "
            r"max_items=(\d+) retained_bytes=(\d+) max_bytes=(\d+)", line)
        if match:
            result["reductions"].append(tuple(int(value) for value in match.groups()))
        match = re.search(
            r"context budget exhausted: items=(\d+) max_items=(\d+) "
            r"bytes=(\d+) max_bytes=(\d+) continuation=(\d+)", line)
        if match:
            result["exhausted"].append(tuple(int(value) for value in match.groups()))
        match = re.search(
            r"stage=(\S+) purpose=(\S+).*?window_tokens=(Some\((\d+)\)|None)"
            r".*?\sestimated_input_tokens=(\d+)", line)
        if match:
            result["budgets"].append({
                "stage": match.group(1),
                "purpose": match.group(2),
                "window": None if match.group(3) == "None" else int(match.group(4)),
                "estimated_input": int(match.group(5)),
            })
        match = re.search(
            r"run finished run=(\S+) outcome=(\w+) error_category=(\S+) "
            r"source_persistence=(\w+) turns=(\d+) calls=(\d+)", line)
        if match:
            result["finished"].append({
                "run": match.group(1),
                "outcome": match.group(2),
                "error_category": match.group(3),
                "persistence": match.group(4),
                "turns": int(match.group(5)),
                "calls": int(match.group(6)),
            })
    return result


def payload_bytes(item):
    fields = ("text", "content", "reasoning", "args", "item_key", "provider_ref",
              "run", "tool", "turn")
    return sum(len(str(item.get(field) or "").encode()) for field in fields)


def source_records(document):
    records = list(document.get("source_records") or [])
    operations = document.get("operations") or []
    for operation in operations:
        if operation.get("kind") == "source_batch":
            records.extend(operation.get("record") or [])
    return records


def session_artifacts(store, session):
    name = session.encode().hex()
    files = [
        store / "session-store.json",
        store / "session-store.operations.jsonl",
        store / "sessions" / f"{name}.json",
        store / "sessions" / f"{name}.operations.jsonl",
    ]
    records = []
    for path in files:
        if not path.is_file():
            continue
        if path.suffix == ".jsonl":
            for line in path.read_text().splitlines():
                value = json.loads(line)
                if value.get("kind") == "source_batch":
                    records.extend(value.get("record") or [])
        else:
            value = json.loads(path.read_text())
            records.extend(value.get("source_records") or [])
            for operation in value.get("operations") or []:
                if operation.get("kind") == "source_batch":
                    records.extend(operation.get("record") or [])
    unique = {}
    for record in records:
        key = (str(record.get("run") or ""), int(record.get("sequence") or 0))
        unique[key] = record
    ordered = [unique[key] for key in sorted(unique)]
    return {
        "records": len(ordered),
        "payload_bytes": sum(payload_bytes(record.get("item") or {}) for record in ordered),
        "tool_results": sum(
            1 for record in ordered
            if (record.get("item") or {}).get("kind") == "tool_result"
        ),
        "unknown_effects": sum(
            1 for record in ordered
            if (record.get("item") or {}).get("effect") == "unknown"
        ),
        "largest_result_bytes": max(
            [len(str((record.get("item") or {}).get("content") or "").encode())
             for record in ordered
             if (record.get("item") or {}).get("kind") == "tool_result"],
            default=0,
        ),
    }


def big_output_prompt(bytes_per_call, calls):
    command = f"head -c {bytes_per_call} /dev/zero | tr '\\\\0' a"
    return (
        "This is an isolated automated context-budget regression in a temporary directory. "
        "The command writes no files. Follow it exactly and do not explain.\n"
        f"Call the host_exec tool {calls} times in one batch. Every call must use exactly:\n"
        f'{{"argv":["sh","-c","{command}"]}}\n'
        "After all results arrive, reply with exactly: BIG OUTPUT DONE"
    )


def check_server_binary(binary):
    if not binary.is_file():
        raise SystemExit(f"nexus-server binary not found: {binary}\n"
                         "Build it with: cargo build --release -p nexus-server --features nexus-server/container --bins")
    return binary.resolve()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server-binary", type=Path, default=Path("target/release/nexus-server"))
    parser.add_argument("--provider", default="opencode-go")
    parser.add_argument("--model", default="deepseek-v4-1-flash")
    parser.add_argument("--calls", type=int, default=6)
    parser.add_argument("--bytes-per-call", type=int, default=180_000)
    parser.add_argument("--timeout", type=int, default=900)
    parser.add_argument("--output-dir", type=Path)
    args = parser.parse_args()
    if args.calls < 2 or args.bytes_per_call < 16_384:
        parser.error("--calls must be at least 2 and --bytes-per-call at least 16384")
    if args.calls * args.bytes_per_call < LOCAL_CONTEXT_BYTES + 64 * 1024:
        parser.error("the requested command results must exceed the local 1 MiB budget")

    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output = (args.output_dir or Path("logs") / f"live-context-endurance-{stamp}").resolve()
    output.mkdir(parents=True, mode=0o700, exist_ok=False)
    root = output / "project"
    # The server creates its store with owner-only permissions; pre-creating it
    # would make the stricter store check fail.
    store = output / "session-store"
    root.mkdir()
    binary = check_server_binary(args.server_binary)

    report = {
        "schema": SCHEMA,
        "status": "running",
        "started_at_utc": stamp,
        "provider": args.provider,
        "model": args.model,
        "limits": {
            "local_context_bytes": LOCAL_CONTEXT_BYTES,
            "calls": args.calls,
            "bytes_per_call": args.bytes_per_call,
        },
        "runs": [],
        "first": None,
        "restored": None,
        "context": None,
        "artifacts": {"server_log": str(output / "server.log"), "directory": str(output)},
    }
    report_path = output / "report.json"
    token_env = "NEXUS_LIVE_CONTEXT_TOKEN"
    server_log = output / "server.log"
    server = None
    session = None
    try:
        port = free_port()
        base = f"http://127.0.0.1:{port}"
        server, token = start_server(binary, port, token_env, root, store, server_log)
        wait_ready(base, token, server)
        session = api(base, token, "/sessions", {
            "provider": args.provider, "model": args.model,
        })["session"]
        print(f"[session] {session}", flush=True)

        prompt = big_output_prompt(args.bytes_per_call, args.calls)
        for attempt in range(1, 5):
            outcome, _ = submit_and_wait(base, token, session, prompt, args.timeout, report)
            first = session_artifacts(store, session)
            print(f"[budget {attempt}] outcome={outcome} records={first['records']} "
                  f"bytes={first['payload_bytes']} tools={first['tool_results']}", flush=True)
            if outcome != "completed":
                raise RuntimeError(f"live run {attempt} did not complete: {outcome}")
            if first["payload_bytes"] >= LOCAL_CONTEXT_BYTES:
                break
            prompt = (
                "The regression still needs more archived command output. "
                f"Call host_exec {args.calls} more times in one batch with exactly "
                f'{{"argv":["sh","-c","head -c {args.bytes_per_call} /dev/zero | tr \'\\\\0\' a"]}}. '
                "Then reply with exactly: BIG OUTPUT DONE"
            )
        report["first"] = {**report["runs"][-1], "session": session, "history": first}
        if first["payload_bytes"] < LOCAL_CONTEXT_BYTES:
            raise RuntimeError("the live runs did not exceed the local context budget")

        log_text = server_log.read_text(errors="replace")
        diagnostics = parse_diagnostics(log_text)
        report["context"] = {
            "reductions": diagnostics["reductions"],
            "exhausted": diagnostics["exhausted"],
            "budgets": diagnostics["budgets"],
            "finished": diagnostics["finished"],
        }
        if diagnostics["exhausted"]:
            raise RuntimeError("the live run still hit the local context limit")
        if not diagnostics["reductions"]:
            raise RuntimeError("no context reduction was recorded after exceeding the local budget")

        stop_server(server)
        port = free_port()
        base = f"http://127.0.0.1:{port}"
        server, token = start_server(binary, port, token_env, root, store, server_log)
        wait_ready(base, token, server)
        restored = api(base, token, f"/sessions/{session}/restore", {
            "provider": args.provider, "model": args.model,
        })
        if restored.get("session") != session:
            raise RuntimeError(f"restore returned a different session: {restored}")
        outcome, _ = submit_and_wait(
            base, token, session,
            "Reply with exactly ENDURANCE-OK and nothing else.", args.timeout, report)
        report["restored"] = {**report["runs"][-1], "history": session_artifacts(store, session)}
        print(f"[restored] outcome={outcome}", flush=True)
        if outcome != "completed":
            raise RuntimeError(f"restored live run did not complete: {outcome}")

        final_text = server_log.read_text(errors="replace")
        diagnostics = parse_diagnostics(final_text)
        report["context"]["reductions"] = diagnostics["reductions"]
        report["context"]["exhausted"] = diagnostics["exhausted"]
        report["context"]["budgets"] = diagnostics["budgets"]
        report["context"]["finished"] = diagnostics["finished"]
        if diagnostics["exhausted"]:
            raise RuntimeError("a local context limit was recorded after restore")
        report["status"] = "passed"
    except Exception as error:  # noqa: BLE001 - report any live or assertion failure
        report["status"] = "failed"
        report["error"] = str(error)
        raise
    finally:
        if server is not None:
            stop_server(server)
        write_json(report_path, report)
        print(json.dumps({
            "status": report["status"],
            "report": str(report_path),
            "session": session,
        }), flush=True)


if __name__ == "__main__":
    main()
