#!/usr/bin/env python3
"""Measure TUI RSS/CPU while a loopback provider streams SSE chunks.

This is an opt-in local workload: it never contacts a live provider and writes
only temporary configuration plus the requested JSON evidence file. Linux
uses /proc counters; macOS uses /bin/ps for sustained workloads and
`proc_pid_rusage` resident-size samples for the short coding workflow.
"""

import argparse
from contextlib import ExitStack
import ctypes
import hashlib
import http.server
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import socketserver
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
from readiness import ProbeScreen


def sample_linux_process(pid):
    status = Path(f"/proc/{pid}/status").read_text()
    rss_kib = high_kib = None
    for line in status.splitlines():
        if line.startswith("VmRSS:"):
            rss_kib = int(line.split()[1])
        elif line.startswith("VmHWM:"):
            high_kib = int(line.split()[1])
    stat = Path(f"/proc/{pid}/stat").read_text()
    fields = stat[stat.rfind(")") + 2:].split()
    cpu_ticks = int(fields[11]) + int(fields[12])
    return {"rss_kib": rss_kib, "vm_hwm_kib": high_kib,
            "cpu_ticks": cpu_ticks, "monotonic_ns": time.monotonic_ns()}


def sample_macos_process(pid, use_rusage=False):
    if use_rusage:
        info_size = 16 + 36 * ctypes.sizeof(ctypes.c_uint64)
        info = ctypes.create_string_buffer(info_size)
        proc = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
        query = proc.proc_pid_rusage
        query.argtypes = (ctypes.c_int, ctypes.c_int, ctypes.c_void_p)
        query.restype = ctypes.c_int
        if query(pid, 4, ctypes.cast(info, ctypes.c_void_p)) != 0:
            error = ctypes.get_errno()
            detail = os.strerror(error) if error else "system error unavailable"
            raise OSError(f"proc_pid_rusage(RUSAGE_INFO_V4) failed: {detail}")
        resident_bytes = ctypes.c_uint64.from_buffer(info, 16 + 6 * 8).value
        return {"rss_kib": (resident_bytes + 1023) // 1024,
                "resident_bytes": resident_bytes,
                "monotonic_ns": time.monotonic_ns()}
    result = subprocess.run(
        ["/bin/ps", "-o", "rss=", "-o", "%cpu=", "-p", str(pid)],
        check=True, capture_output=True, text=True, timeout=2)
    fields = result.stdout.split()
    if len(fields) != 2:
        raise RuntimeError(f"unexpected ps sample for PID {pid}: {result.stdout!r}")
    return {"rss_kib": int(fields[0]), "ps_cpu_percent": float(fields[1]),
            "monotonic_ns": time.monotonic_ns()}


def sample_process(pid, use_fast_macos_rusage=False):
    if sys.platform.startswith("linux"):
        return sample_linux_process(pid)
    return sample_macos_process(pid, use_rusage=use_fast_macos_rusage)


def visible_resource_index(screen):
    text = "\n".join("".join(row) for row in screen.cells)
    matches = re.findall(r"resource-(\d{6})", text)
    return max(map(int, matches)) if matches else None


def coding_payload_chunks(count, workflow_detail=None):
    lines = [
        "# Proposed patch: validate workspace change targets\n\n",
        "This synthetic response reviews paths before applying a generated patch.\n\n",
        "```rust\n",
        "use std::path::{Component, Path};\n\n",
        "#[derive(Debug, PartialEq, Eq)]\n",
        "pub enum TargetError { Empty, Absolute, ParentTraversal, InvalidEncoding }\n\n",
        "pub fn validate_target(path: &str) -> Result<(), TargetError> {\n",
        "    if path.is_empty() { return Err(TargetError::Empty); }\n",
        "    let candidate = Path::new(path);\n",
        "    if candidate.is_absolute() { return Err(TargetError::Absolute); }\n",
        "    for component in candidate.components() {\n",
        "        if matches!(component, Component::ParentDir) {\n",
        "            return Err(TargetError::ParentTraversal);\n",
        "        }\n",
        "    }\n",
        "    Ok(())\n",
        "}\n\n",
    ]
    for index in range(12):
        lines.extend([
            f"// Validate generated patch section {index:02d} before staging.\n",
            f"pub fn validate_section_{index:02d}(path: &str) -> Result<(), TargetError> {{\n",
            "    validate_target(path)?;\n",
            "    let normalized = Path::new(path).components().collect::<Vec<_>>();\n",
            "    if normalized.is_empty() { return Err(TargetError::Empty); }\n",
            "    Ok(())\n",
            "}\n\n",
        ])
    lines.extend([
        "```\n\n",
        "The caller should validate every target before opening any destination.\n",
        "This keeps malformed patches from causing partial workspace updates.\n",
    ])
    if workflow_detail is not None:
        lines.append(f"{workflow_detail}\n")
    lines.append(f"// resource-{count - 1:06d}\n")
    payload = "".join(lines)
    boundaries = [(len(payload) * index) // count for index in range(count + 1)]
    chunks = [payload[boundaries[index]:boundaries[index + 1]]
              for index in range(count)]
    if any(not chunk for chunk in chunks):
        raise ValueError("coding payload produced an empty SSE chunk")
    return chunks, len(payload.encode("utf-8"))


def coding_tool_call_events():
    """SSE events for the deterministic read-only host_read call."""
    events = [
        {"choices": [{"index": 0, "delta": {"role": "assistant",
                                                  "content": "Inspecting src/lib.rs. "},
                      "finish_reason": None}]},
        {"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0,
             "id": "fixture-read-1", "type": "function",
             "function": {"name": "host_read", "arguments": "{\"path\":"}}]},
                      "finish_reason": None}]},
        {"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0,
             "function": {"arguments": "\"src/lib.rs\"}"}}]},
                      "finish_reason": None}]},
        {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
    ]
    return [*(f"data: {json.dumps(event)}\n\n".encode() for event in events),
            b"data: [DONE]\n\n"]


def coding_tool_result(request_payload):
    """Validate that the provider's follow-up includes the actual tool result."""
    messages = request_payload.get("messages", [])
    for message in messages:
        if (message.get("role") == "tool"
                and message.get("tool_call_id") == "fixture-read-1"):
            content = message.get("content")
            if isinstance(content, str) and "fn validate_target" in content:
                return content
    raise AssertionError("follow-up request did not contain the host_read result")


def stop_target(proc):
    if proc.poll() is not None:
        return {"method": "already-exited", "returncode": proc.returncode,
                "escalated": False}
    method = "sigterm-process-group"
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except PermissionError:
        method = "sigterm-direct-child"
        proc.terminate()
    escalated = False
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        escalated = True
        method = "sigkill-process-group"
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except PermissionError:
            method = "sigkill-direct-child"
            proc.kill()
        proc.wait(timeout=5)
    return {"method": method, "returncode": proc.returncode,
            "escalated": escalated}


def measure(binary, run_index, window_s, sample_interval_s, finite_chunks=None,
            payload_profile="numbered"):
    stop_provider = threading.Event()
    sent = []
    requests = []
    request_payloads = []
    errors = []
    workflow_tool_results = []
    content_chunks = None
    payload_bytes = None
    if finite_chunks is not None and payload_profile == "coding":
        content_chunks, payload_bytes = coding_payload_chunks(finite_chunks)
    elif finite_chunks is not None and payload_profile != "coding-workflow":
        payload_bytes = finite_chunks * len("resource-000000 ".encode())

    class Provider(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            nonlocal content_chunks, payload_bytes
            try:
                assert self.path == "/v1/chat/completions", self.path
                requests.append(time.monotonic_ns())
                size = int(self.headers["Content-Length"])
                assert 0 < size <= 1024 * 1024, size
                request_payload = json.loads(self.rfile.read(size))
                request_payloads.append(request_payload)
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                if payload_profile == "coding-workflow" and len(request_payloads) == 1:
                    for event in coding_tool_call_events():
                        self.wfile.write(event)
                        self.wfile.flush()
                        stop_provider.wait(0.002)
                    return
                if payload_profile == "coding-workflow":
                    assert len(request_payloads) == 2, (
                        "expected one tool call and one follow-up request",
                        len(request_payloads))
                    tool_result = coding_tool_result(request_payload)
                    tool_result_bytes = tool_result.encode("utf-8")
                    workflow_tool_results.append({
                        "bytes": len(tool_result_bytes),
                        "sha256": hashlib.sha256(tool_result_bytes).hexdigest(),
                    })
                    content_chunks, payload_bytes = coding_payload_chunks(
                        finite_chunks,
                        workflow_detail=(
                            f"Inspected local src/lib.rs ({len(tool_result_bytes)} bytes; "
                            f"sha256 {workflow_tool_results[-1]['sha256']})"),
                    )
                index = 0
                while (not stop_provider.is_set()
                       and (finite_chunks is None or index < finite_chunks)):
                    content = (content_chunks[index] if content_chunks is not None else
                               f"resource-{index:06d} ")
                    chunk = {"id": "resource-pressure", "object": "chat.completion.chunk",
                             "choices": [{"index": 0, "delta": {"content": content},
                                  "finish_reason": ("stop" if finite_chunks is not None
                                                    and index + 1 == finite_chunks else None)}]}
                    self.wfile.write(("data: " + json.dumps(chunk) + "\n\n").encode())
                    self.wfile.flush()
                    sent.append((time.monotonic_ns(), index))
                    index += 1
                    stop_provider.wait(0.001)
                if finite_chunks is not None and not stop_provider.is_set():
                    self.wfile.write(b"data: [DONE]\n\n")
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                pass
            except Exception as error:
                errors.append(repr(error))

    class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
        daemon_threads = True
        allow_reuse_address = True

    server = Server(("127.0.0.1", 0), Provider)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    proc = None
    master = slave = None
    try:
        with ExitStack() as cleanup:
            root = cleanup.enter_context(tempfile.TemporaryDirectory(
                prefix="nexus-stream-resources-"))
            for name in ("config", "state", "data", "cache"):
                (Path(root) / name).mkdir(mode=0o700)
            if payload_profile == "coding-workflow":
                source = Path(root) / "src/lib.rs"
                source.parent.mkdir()
                source.write_text(
                    "pub fn validate_target(path: &str) -> bool {\n"
                    "    !path.is_empty() && !path.starts_with('/') && !path.contains(\"..\")\n"
                    "}\n"
                )
            endpoint = f"http://127.0.0.1:{server.server_port}/v1"
            config = {
                "revision": 2,
                "providers": [{"id": "local", "display_name": "Loopback fixture",
                               "adapter": "direct", "protocol": "chat_completions",
                               "endpoint": endpoint,
                               "credential": {"env": "NEXUS_RESOURCE_TEST_KEY"},
                               "default_model": "model"}],
                "models": [{"id": "model-id", "provider": "local", "name": "model"}],
                "favourites": [], "recent": [],
            }
            config_path = Path(root) / "config.json"
            config_path.write_text(json.dumps(config))
            env = {"HOME": root, "XDG_CONFIG_HOME": root + "/config",
                   "XDG_STATE_HOME": root + "/state", "XDG_DATA_HOME": root + "/data",
                   "XDG_CACHE_HOME": root + "/cache", "NEXUS_CONFIG": str(config_path),
                   "NEXUS_RESOURCE_TEST_KEY": "loopback-only", "TERM": "xterm-256color",
                   "PATH": os.environ.get("PATH", "/usr/bin:/bin")}
            master, slave = pty.openpty()
            fcntl = __import__("fcntl")
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
            proc = subprocess.Popen([sys.executable, __file__, "--controlling-child", binary],
                                    stdin=slave, stdout=slave, stderr=slave,
                                    env=env, cwd=root, start_new_session=True)
            cleanup.callback(stop_target, proc)
            os.close(slave)
            slave = None
            screen = ProbeScreen(120, 40)
            captured = bytearray()

            def pump_until(condition, timeout_s, label):
                deadline = time.monotonic() + timeout_s
                while not condition():
                    assert proc.poll() is None, (label, proc.returncode, captured[-2000:])
                    assert time.monotonic() < deadline, (label, captured[-2000:])
                    ready, _, _ = select.select([master], [], [], 0.02)
                    if master in ready:
                        data = os.read(master, 65536)
                        screen.feed(data)
                        captured.extend(data)
                        del captured[:-65536]

            pump_until(lambda: screen.contains("composer (fixed)"), 10, "composer")
            coding_workflow = payload_profile == "coding-workflow"
            workflow_start_ns = None
            if coding_workflow:
                samples = [sample_process(proc.pid, use_fast_macos_rusage=True)]
                workflow_start_ns = time.monotonic_ns()
                os.write(master, b"Inspect src/lib.rs and summarize its validation.\r")
                pump_until(
                    lambda: len(requests) == 2 and len(sent) >= min(32, finite_chunks),
                    15,
                    "tool result and final response stream",
                )
                os.write(master, b"input-probe-under-load")
                input_probe_started_ns = time.monotonic_ns()
            else:
                os.write(master, b"measure loopback streaming memory\r")
                pump_until(lambda: len(sent) >= min(32, finite_chunks or 32),
                           10, "provider stream start")
                samples = []
                input_probe_started_ns = None

            visible_progress = []
            initial_visible_index = visible_resource_index(screen)
            if initial_visible_index is not None:
                visible_progress.append((time.monotonic_ns(), initial_visible_index))
            samples.append(sample_process(
                proc.pid, use_fast_macos_rusage=coding_workflow))
            start_ns = workflow_start_ns if coding_workflow else sent[0][0]
            deadline_ns = (start_ns + int(15 * 1_000_000_000)
                           if coding_workflow else
                           start_ns + int(window_s * 1_000_000_000))
            sample_interval_ns = int(sample_interval_s * 1_000_000_000)
            next_sample_ns = start_ns
            input_probe_visible_ms = None
            while True:
                now_ns = time.monotonic_ns()
                final_marker_seen = (finite_chunks is not None
                                     and screen.contains(f"resource-{finite_chunks - 1:06d}"))
                if final_marker_seen or (finite_chunks is None and now_ns >= deadline_ns):
                    break
                assert now_ns < deadline_ns, (
                    "finite stream did not render its final marker before the window deadline",
                    visible_resource_index(screen), sent[-1] if sent else None,
                    captured[-1000:])
                assert proc.poll() is None, ("target exited during stream", proc.returncode)
                assert not errors, errors
                if now_ns >= next_sample_ns:
                    samples.append(sample_process(
                        proc.pid, use_fast_macos_rusage=coding_workflow))
                    next_sample_ns = time.monotonic_ns() + sample_interval_ns
                if (coding_workflow and input_probe_visible_ms is None
                        and screen.contains("input-probe-under-load")):
                    input_probe_visible_ms = (
                        time.monotonic_ns() - input_probe_started_ns
                    ) / 1_000_000
                wait_ns = min(deadline_ns, next_sample_ns) - time.monotonic_ns()
                ready, _, _ = select.select([master], [], [], max(0, wait_ns) / 1_000_000_000)
                if master in ready:
                    data = os.read(master, 65536)
                    screen.feed(data)
                    if (coding_workflow and input_probe_visible_ms is None
                            and screen.contains("input-probe-under-load")):
                        input_probe_visible_ms = (
                            time.monotonic_ns() - input_probe_started_ns
                        ) / 1_000_000
                    visible_index = visible_resource_index(screen)
                    if visible_index is not None and (
                            not visible_progress or visible_index > visible_progress[-1][1]):
                        visible_progress.append((time.monotonic_ns(), visible_index))
                    captured.extend(data)
                    del captured[:-65536]
            end_ns = time.monotonic_ns()
            if coding_workflow:
                pump_until(
                    lambda: screen.contains("Completed") or screen.contains("completed"),
                    5,
                    "completed run terminal",
                )
            if finite_chunks is None:
                os.write(master, b"\x03")
                pump_until(lambda: screen.contains("Cancelled"), 10, "cancel terminal")
            assert sent, "provider sent no chunks"
            assert not errors, errors
            if finite_chunks is not None:
                expected_requests = 2 if coding_workflow else 1
                assert len(requests) == expected_requests, (
                    f"expected {expected_requests} finite-stream requests, observed {len(requests)}")
                assert len(sent) == finite_chunks, ("finite fixture chunk count mismatch",
                                                    len(sent), finite_chunks)
            if coding_workflow:
                assert len(workflow_tool_results) == 1, "read-only tool result was not observed"
                assert input_probe_visible_ms is not None, (
                    "input sentinel was not rendered during the coding workflow")

            rss = [item["rss_kib"] for item in samples if item["rss_kib"] is not None]
            elapsed_s = (end_ns - start_ns) / 1_000_000_000
            result = {"run": run_index, "valid": True,
                    "outcome": ("coding-tool-workflow-rendered" if coding_workflow else
                                "final-marker-rendered" if finite_chunks is not None
                                else "cancelled-after-stream-window"),
                    "window_s": elapsed_s, "provider_chunks_sent": len(sent),
                    "provider_requests": len(requests),
                    "payload_profile": payload_profile,
                    "payload_bytes": payload_bytes,
                    "sample_count": len(samples), "sample_interval_s": sample_interval_s,
                    "rss_min_kib": min(rss), "rss_median_kib": sorted(rss)[len(rss)//2],
                    "rss_max_sampled_kib": max(rss),
                    "rss_peak_method": ("/proc VmHWM; current RSS sampling paced by monotonic interval"
                                         if sys.platform.startswith("linux") else
                                         "maximum of sampled proc_pid_rusage RUSAGE_INFO_V4 ri_resident_size values"
                                         if coding_workflow else
                                         "maximum of sampled /bin/ps RSS values")}
            if finite_chunks is None:
                result["provider_chunks_per_s"] = len(sent) / elapsed_s
            elif coding_workflow:
                final_response_s = (end_ns - requests[1]) / 1_000_000_000
                result.update({
                    "submission_to_final_marker_s": elapsed_s,
                    "final_response_to_marker_s": final_response_s,
                    "final_response_visible_chunks_per_s": len(sent) / final_response_s
                    if final_response_s > 0 else None,
                    "provider_tool_requests": 1,
                    "provider_followup_requests": 1,
                    "tool_name": "host_read",
                    "tool_path": "src/lib.rs",
                    "tool_result_bytes": workflow_tool_results[0]["bytes"],
                    "tool_result_sha256": workflow_tool_results[0]["sha256"],
                    "input_probe_to_visible_ms": input_probe_visible_ms,
                    "input_probe_rendered_during_workflow": True,
                })
            else:
                result["first_chunk_to_final_marker_s"] = elapsed_s
                result["visible_completion_chunks_per_s"] = len(sent) / elapsed_s
            result["visible_progress_observations"] = len(visible_progress)
            screen_text = "\n".join("".join(row) for row in screen.cells)
            result["viewport_scrollback_visible"] = "presentation-truncated" in screen_text
            result["entry_output_truncation_visible"] = "output truncated" in screen_text
            if len(visible_progress) >= 2:
                progress_s = (visible_progress[-1][0] - visible_progress[0][0]) / 1_000_000_000
                progress_chunks = visible_progress[-1][1] - visible_progress[0][1]
                result.update({"visible_resource_index_first": visible_progress[0][1],
                               "visible_resource_index_last": visible_progress[-1][1],
                               "visible_progress_window_s": progress_s,
                               "visible_marker_advance_per_s": progress_chunks / progress_s
                               if progress_s > 0 else None,
                               "visible_progress_method": "observed monotonic advance of numbered content on the rendered PTY screen; rate covers only first-to-last observed advance"})
            else:
                result["visible_progress_method"] = "insufficient distinct numbered content observations"
            sample_gaps_ms = [
                (current["monotonic_ns"] - previous["monotonic_ns"]) / 1_000_000
                for previous, current in zip(samples, samples[1:])
            ]
            if sample_gaps_ms:
                ordered_gaps = sorted(sample_gaps_ms)
                result.update({"sample_gap_min_ms": ordered_gaps[0],
                               "sample_gap_median_ms": ordered_gaps[len(ordered_gaps) // 2],
                               "sample_gap_max_ms": ordered_gaps[-1]})
            if sys.platform.startswith("linux"):
                hwm = [item["vm_hwm_kib"] for item in samples
                       if item["vm_hwm_kib"] is not None]
                ticks = [item["cpu_ticks"] for item in samples]
                hz = os.sysconf(os.sysconf_names["SC_CLK_TCK"])
                cpu_s = (ticks[-1] - ticks[0]) / hz if len(ticks) > 1 else 0.0
                result.update({"vm_hwm_max_sampled_kib": max(hwm),
                               "cpu_seconds_sampled": cpu_s,
                               "cpu_percent_sampled": cpu_s / elapsed_s * 100,
                               "cpu_measurement_method": "delta of /proc CPU ticks over stream window",
                               "cpu_clock_ticks_per_second": hz})
            elif coding_workflow:
                result["cpu_measurement_method"] = (
                    "not measured for this short composed workflow; see separate idle CPU evidence")
            else:
                ps_cpu = [item["ps_cpu_percent"] for item in samples]
                result.update({"ps_cpu_percent_min": min(ps_cpu),
                               "ps_cpu_percent_median": sorted(ps_cpu)[len(ps_cpu)//2],
                               "ps_cpu_percent_max": max(ps_cpu),
                                "cpu_measurement_method": "OS-reported ps %cpu values; not a window CPU delta"})
            if coding_workflow:
                os.write(master, b"\x03\x03")
                cleanup_deadline = time.monotonic() + 3
                while proc.poll() is None and time.monotonic() < cleanup_deadline:
                    ready, _, _ = select.select([master], [], [], 0.05)
                    if master in ready:
                        try:
                            data = os.read(master, 65536)
                        except OSError:
                            break
                        if not data:
                            break
                        screen.feed(data)
                if proc.poll() is None:
                    result["cleanup"] = stop_target(proc)
                else:
                    result["cleanup"] = {
                        "method": "ctrl-c-pty",
                        "returncode": proc.returncode,
                        "escalated": False,
                    }
            return result
    finally:
        stop_provider.set()
        if master is not None:
            os.close(master)
        if slave is not None:
            os.close(slave)
        server.shutdown()
        server.server_close()
        worker.join(timeout=5)
        assert not worker.is_alive(), "loopback provider did not stop"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--runs", type=int, default=10)
    parser.add_argument("--window", type=float, default=3.0)
    parser.add_argument("--sample-interval", type=float, default=0.02)
    parser.add_argument("--finite-chunks", type=int,
                        help="send a finite SSE stream and measure through its final rendered marker")
    parser.add_argument("--payload-profile",
                        choices=("numbered", "coding", "coding-workflow"),
                        default="numbered",
                        help="synthetic response shape; coding-workflow includes a local read-only tool turn")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not (sys.platform.startswith("linux") or sys.platform == "darwin"):
        raise SystemExit("this resource sampler supports Linux /proc and macOS libproc/ps")
    if (args.runs < 1 or args.window <= 0 or args.sample_interval <= 0
            or (args.finite_chunks is not None and args.finite_chunks < 32)):
        parser.error("runs, window, and sample interval must be positive; finite-chunks must be at least 32")
    if args.payload_profile != "numbered" and args.finite_chunks is None:
        parser.error("coding payload profile requires --finite-chunks")
    if args.payload_profile == "coding-workflow" and args.finite_chunks < 32:
        parser.error("coding workflow requires at least 32 final-response chunks")
    binary = args.binary.resolve(strict=True)
    results = []
    for index in range(args.runs):
        try:
            results.append(measure(binary, index, args.window, args.sample_interval,
                                   args.finite_chunks, args.payload_profile))
        except Exception as error:
            results.append({"run": index, "valid": False, "outcome": "measurement-error",
                            "error": f"{type(error).__name__}: {error}"})
    document = {"binary": str(binary), "binary_sha256": __import__("hashlib").sha256(
                    binary.read_bytes()).hexdigest(),
                "platform": (
                    "Linux /proc process sampling" if sys.platform.startswith("linux") else
                    "macOS proc_pid_rusage RUSAGE_INFO_V4 resident-size sampling"
                    if args.payload_profile == "coding-workflow" else
                    "macOS /bin/ps process sampling"),
                "build": {"profile": "release", "toolchain": "rustc/cargo 1.99.0",
                          "features": "workspace default features",
                          "locked": True},
                "workload": (
                    "loopback Chat Completions: host_read of a temporary Rust fixture, "
                    "tool-result follow-up, finite Rust response and input probe under load"
                    if args.payload_profile == "coding-workflow" else
                    f"loopback HTTP SSE; finite {args.payload_profile} payload through final rendered marker"
                    if args.finite_chunks is not None else
                    "loopback HTTP SSE; sustained streaming then cancellation"),
                "finite_chunks": args.finite_chunks,
                "valid_samples": sum(row["valid"] for row in results),
                "invalid_samples": sum(not row["valid"] for row in results),
                "samples": results}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(document, indent=2) + "\n")
    for row in results:
        print(json.dumps(row, sort_keys=True), flush=True)
    print(f"wrote {args.output}", flush=True)
    if any(not row["valid"] for row in results):
        raise SystemExit(1)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--controlling-child":
        import fcntl
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)
        os.execv(sys.argv[2], [sys.argv[2]])
    main()
