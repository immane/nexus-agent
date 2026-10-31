"""Exercise corrupt metadata using only isolated files and a loopback listener."""

import errno
import fcntl
import json
import os
import pty
import select
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time


def check(binary, corrupt_index):
    with tempfile.TemporaryDirectory(prefix="nexus-startup-failure-") as root:
        home = os.path.join(root, "home")
        state = os.path.join(root, "state")
        config_dir = os.path.join(root, "config")
        for directory in (home, state, config_dir):
            os.mkdir(directory, 0o700)
        base = (os.path.join(home, "Library", "Application Support")
                if sys.platform == "darwin" else state)
        store = os.path.join(base, "nexus-agent", "sessions")
        bodies = os.path.join(store, "sessions")
        os.makedirs(bodies, mode=0o700)
        os.chmod(store, 0o700)
        index = os.path.join(store, "session-index.json")
        journal = os.path.join(store, "session-pending.json")
        staging = os.path.join(bodies, "pending-body.json")
        body = os.path.join(bodies, "686973746f7279.json")
        fixtures = {
            index: b"{broken index" if corrupt_index else b'{"revision":1,"sessions":[]}',
            journal: b"{broken journal",
            staging: b"staging sentinel: do not publish or discard",
            body: b"historical body sentinel: do not restore",
        }
        for path, contents in fixtures.items():
            fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, "wb") as file:
                file.write(contents)

        credentials = os.path.join(config_dir, "nexus-agent")
        os.mkdir(credentials, 0o700)
        fifo = os.path.join(credentials, "credentials.json")
        os.mkfifo(fifo, 0o600)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            config = {"revision": 2, "providers": [{"id": "p", "display_name": "Provider",
                      "adapter": "direct", "protocol": "chat_completions",
                      "endpoint": "http://127.0.0.1:%d/v1" % listener.getsockname()[1],
                      "credential": {"stored": "p"}, "default_model": "model"}],
                      "models": [{"id": "m", "provider": "p", "name": "model"}],
                      "favourites": [], "recent": []}
            config_path = os.path.join(root, "config.json")
            with open(config_path, "w", encoding="utf-8") as file:
                json.dump(config, file)
            os.chmod(config_path, 0o600)
            env = {"HOME": home, "XDG_CONFIG_HOME": config_dir, "XDG_STATE_HOME": state,
                   "XDG_DATA_HOME": os.path.join(root, "data"),
                   "XDG_CACHE_HOME": os.path.join(root, "cache"),
                   "NEXUS_CONFIG": config_path, "TERM": "xterm-256color"}
            master, slave = pty.openpty()
            proc = None
            try:
                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 120, 0, 0))
                proc = subprocess.Popen([binary], stdin=slave, stdout=slave, stderr=slave,
                                        env=env, cwd=root)
                os.close(slave)
                slave = None
                output = b""
                deadline = time.monotonic() + 15
                while True:
                    try:
                        writer = os.open(fifo, os.O_WRONLY | os.O_NONBLOCK)
                    except OSError as error:
                        assert error.errno == errno.ENXIO, error
                    else:
                        os.close(writer)
                        raise AssertionError("startup opened the credential FIFO")
                    ready, _, _ = select.select([master, listener], [], [], 0.02)
                    assert listener not in ready, "startup contacted the provider"
                    if master in ready:
                        try:
                            chunk = os.read(master, 65536)
                        except OSError as error:
                            if error.errno != errno.EIO:
                                raise
                            chunk = b""
                        output += chunk
                    if proc.poll() is not None and (master not in ready or not chunk):
                        break
                    assert time.monotonic() < deadline, ("startup timed out", output)
                assert proc.returncode != 0, ("startup unexpectedly succeeded", output)
                expected = b"session index is invalid" if corrupt_index else b"session transaction is invalid"
                assert expected in output, (expected, output)
                assert b"\x1b[?25l" not in output, ("entered interactive UI", output)
                assert b"historical body sentinel" not in output, output
                for path, contents in fixtures.items():
                    with open(path, "rb") as file:
                        assert file.read() == contents, ("fixture changed", path)
                assert set(os.listdir(bodies)) == {os.path.basename(staging), os.path.basename(body)}
            finally:
                if slave is not None:
                    os.close(slave)
                if proc is not None and proc.poll() is None:
                    proc.terminate()
                    try:
                        proc.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait(timeout=5)
                os.close(master)


for corrupt_index in (True, False):
    check(sys.argv[1], corrupt_index)
