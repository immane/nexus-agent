"""Startup must not decode or display an old body; this cannot rule out reads."""

import fcntl
import errno
import json
import os
import pty
import select
import struct
import subprocess
import sys
import tempfile
import termios
import time


with tempfile.TemporaryDirectory(prefix="nexus-history-startup-") as root:
    home = os.path.join(root, "home")
    state = os.path.join(root, "state")
    os.mkdir(home, 0o700)
    os.mkdir(state, 0o700)
    base = (os.path.join(home, "Library", "Application Support")
            if sys.platform == "darwin" else state)
    store = os.path.join(base, "nexus-agent", "sessions")
    bodies = os.path.join(store, "sessions")
    os.makedirs(bodies, mode=0o700)
    os.chmod(store, 0o700)
    session = "historical-session"
    sentinel = b"UNIQUE_OLD_BODY_SENTINEL_942813"
    body = b"invalid session body: " + sentinel
    body_path = os.path.join(bodies, session.encode().hex() + ".json")
    index_path = os.path.join(store, "session-index.json")
    for path, contents in ((body_path, body), (index_path, json.dumps({
            "revision": 1, "sessions": [{"session": session, "logical_revision": 1}]
    }).encode())):
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "wb") as file:
            file.write(contents)

    config_dir = os.path.join(root, "config")
    credential_dir = os.path.join(config_dir, "nexus-agent")
    os.makedirs(credential_dir, mode=0o700)
    credential_path = os.path.join(credential_dir, "credentials.json")
    os.mkfifo(credential_path, 0o600)
    config_path = os.path.join(root, "config.json")
    with open(config_path, "w", encoding="utf-8") as file:
        json.dump({"revision": 2, "providers": [{"id": "p", "display_name": "Provider",
                  "adapter": "direct", "protocol": "chat_completions",
                  "endpoint": "http://127.0.0.1:1/v1", "credential": {"stored": "p"},
                  "default_model": "model"}],
                  "models": [{"id": "m", "provider": "p", "name": "model"}],
                  "favourites": [], "recent": []}, file)
    os.chmod(config_path, 0o600)
    env = {"HOME": home, "XDG_CONFIG_HOME": config_dir,
           "XDG_STATE_HOME": state, "XDG_DATA_HOME": os.path.join(root, "data"),
           "XDG_CACHE_HOME": os.path.join(root, "cache"),
           "NEXUS_CONFIG": config_path,
           "TERM": "xterm-256color"}
    master, slave = pty.openpty()
    try:
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 120, 0, 0))
        proc = subprocess.Popen([sys.argv[1]], stdin=slave, stdout=slave,
                                stderr=slave, env=env, cwd=root)
    finally:
        os.close(slave)
    try:
        output = b""
        deadline = time.monotonic() + 15
        while b"\x1b[?25l" not in output:
            assert proc.poll() is None, ("exited before first frame", output)
            assert time.monotonic() < deadline, ("startup timed out", output)
            if select.select([master], [], [], 0.1)[0]:
                output += os.read(master, 65536)
        # A submission would block opening the credential FIFO before network access.
        end = time.monotonic() + 1
        while time.monotonic() < end:
            assert proc.poll() is None, ("unexpected exit", output)
            if select.select([master], [], [], 0.1)[0]:
                output += os.read(master, 65536)
        assert sentinel not in output, output
        assert b"canned M0 submission accepted" not in output, output
        try:
            writer = os.open(credential_path, os.O_WRONLY | os.O_NONBLOCK)
        except OSError as error:
            assert error.errno == errno.ENXIO, error
        else:
            os.close(writer)
            raise AssertionError("startup opened the credential FIFO")
        with open(body_path, "rb") as file:
            assert file.read() == body, "historical body changed"
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)
        os.close(master)
