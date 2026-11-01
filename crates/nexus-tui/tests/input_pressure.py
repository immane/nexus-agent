"""Real PTY input during bounded loopback SSE traffic; no live provider calls."""

from contextlib import ExitStack
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import re
import secrets
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "tools" / "perf"))
from readiness import ProbeScreen


def stop_target(proc):
    if proc.poll() is not None:
        return
    os.killpg(proc.pid, signal.SIGTERM)
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait(timeout=5)


def check(binary):
    stop = threading.Event()
    sent = []
    errors = []

    class Provider(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            try:
                assert self.path == "/v1/chat/completions", self.path
                size = int(self.headers["Content-Length"])
                assert 0 < size <= 1024 * 1024, size
                self.rfile.read(size)
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                for index in range(10000):
                    if stop.is_set():
                        break
                    chunk = {"id": "pressure", "object": "chat.completion.chunk",
                             "choices": [{"index": 0, "delta": {
                                 "content": "stream-%04d " % index},
                                 "finish_reason": None}]}
                    self.wfile.write(("data: " + json.dumps(chunk) + "\n\n").encode())
                    self.wfile.flush()
                    sent.append(index)
                    stop.wait(0.001)
            except (BrokenPipeError, ConnectionResetError):
                # Cancellation closes the provider connection deliberately.
                pass
            except Exception as error:
                errors.append(repr(error))

    server = http.server.HTTPServer(("127.0.0.1", 0), Provider)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    proc = None
    master = slave = None
    try:
        with ExitStack() as cleanup:
            root = cleanup.enter_context(tempfile.TemporaryDirectory(prefix="nexus-input-pressure-"))
            config = {
                "revision": 2,
                "providers": [{"id": "p", "display_name": "Local fixture",
                               "adapter": "direct", "protocol": "chat_completions",
                               "endpoint": "http://127.0.0.1:%d/v1" % server.server_port,
                               "credential": {"env": "TEST_PROVIDER_KEY"},
                               "default_model": "model"}],
                "models": [{"id": "m", "provider": "p", "name": "model"}],
                "favourites": [], "recent": [],
            }
            path = Path(root) / "config.json"
            path.write_text(json.dumps(config))
            env = {"HOME": root, "XDG_CONFIG_HOME": root + "/config",
                   "XDG_STATE_HOME": root + "/state", "XDG_DATA_HOME": root + "/data",
                   "XDG_CACHE_HOME": root + "/cache", "NEXUS_CONFIG": str(path),
                   "TERM": "xterm-256color", "TEST_PROVIDER_KEY": "synthetic",
                   "PATH": os.environ.get("PATH", "/usr/bin:/bin")}
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
            proc = subprocess.Popen([binary], stdin=slave, stdout=slave, stderr=slave,
                                    env=env, cwd=root, start_new_session=True)
            cleanup.callback(stop_target, proc)
            os.close(slave)
            slave = None
            screen = ProbeScreen(120, 40)
            captured = bytearray()

            def latest_visible_fragment():
                indices = re.findall(r"stream-(\d+)", "\n".join(
                    "".join(row) for row in screen.cells))
                return max(map(int, indices), default=-1)

            def await_condition(condition, label):
                deadline = time.monotonic() + 10
                while not condition():
                    assert not errors, errors
                    assert proc.poll() is None, (label, proc.returncode, captured[-2000:])
                    assert time.monotonic() < deadline, (label, captured[-2000:])
                    if select.select([master], [], [], 0.05)[0]:
                        data = os.read(master, 65536)
                        screen.feed(data)
                        captured.extend(data)
                        del captured[:-65536]

            await_condition(lambda: screen.contains("composer (fixed)"), "first frame")
            assert not termios.tcgetattr(master)[3] & termios.ECHO, "PTY echo must be off"
            os.write(master, b"exercise streaming pressure\r")
            await_condition(lambda: len(sent) >= 32 and screen.contains("stream-"),
                            "visible streaming output")
            # Eighty characters exceed the frontend's 32-key batch bound but
            # fit on one composer row. Verify the entire ordered burst, not echo.
            marker = secrets.token_hex(40)
            before = len(sent)
            started = time.monotonic()
            os.write(master, marker.encode())
            await_condition(lambda: screen.contains(marker), "complete composer burst")
            elapsed = (time.monotonic() - started) * 1000
            assert not termios.tcgetattr(master)[3] & termios.ECHO
            await_condition(lambda: latest_visible_fragment() > before + 32,
                            "frontend output progress during input")
            os.write(master, b"\x03")  # Clear the draft before requesting cancellation.
            await_condition(lambda: not screen.contains(marker), "clear composer")
            os.write(master, b"\x03")
            cancel_started = time.monotonic()
            await_condition(lambda: screen.contains("Cancelled"), "cancelled terminal outcome")
            assert not errors, errors
            print(json.dumps({"burst_chars": len(marker), "burst_response_ms": elapsed,
                              "provider_fragments": len(sent), "cancelled": True,
                              "cancel_response_ms": (time.monotonic() - cancel_started) * 1000}),
                  flush=True)
    finally:
        stop.set()
        for fd in (master, slave):
            if fd is not None:
                os.close(fd)
        server.shutdown()
        server.server_close()
        worker.join(timeout=5)
        assert not worker.is_alive(), "provider fixture failed to stop"


if __name__ == "__main__":
    check(sys.argv[1])
