import fcntl
import json
import os
import pty
import re
import select
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time


def check(binary, demo, reference):
    with tempfile.TemporaryDirectory(prefix="nexus-credential-startup-") as root:
        config_dir = os.path.join(root, "nexus-agent")
        os.mkdir(config_dir)
        os.mkfifo(os.path.join(config_dir, "credentials.json"))
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        config = {
            "revision": 2,
            "providers": [{"id": "p", "display_name": "Provider", "adapter": "direct",
                           "protocol": "chat_completions",
                           "endpoint": "http://127.0.0.1:%d/v1" % listener.getsockname()[1],
                           "credential": reference, "default_model": "model"}],
            "models": [{"id": "m", "provider": "p", "name": "model"}],
            "favourites": [], "recent": [],
        }
        path = os.path.join(root, "config.json")
        with open(path, "w", encoding="utf-8") as file:
            json.dump(config, file)
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 120, 0, 0))
        env = {"HOME": root, "XDG_CONFIG_HOME": root, "NEXUS_CONFIG": path,
               "TERM": "xterm-256color", "TEST_PROVIDER_KEY": "synthetic"}
        proc = subprocess.Popen([binary] + (["--demo"] if demo else []),
                                stdin=slave, stdout=slave, stderr=slave, env=env, cwd=root)
        os.close(slave)
        try:
            output = b""
            deadline = time.monotonic() + 15
            while not re.search(rb"wiring:.*selected.*model.*\(m\).*provider.*validated.*submission", output, re.S):
                assert proc.poll() is None, ("exited before wiring notice", output)
                assert time.monotonic() < deadline, ("startup blocked", output)
                if select.select([master], [], [], 0.1)[0]:
                    output += os.read(master, 65536)
            # Allow an erroneous automatic submission to reach the listener or FIFO.
            end = time.monotonic() + 1
            while time.monotonic() < end:
                ready, _, _ = select.select([master, listener], [], [], 0.1)
                assert listener not in ready, "startup contacted the provider"
                if master in ready:
                    output += os.read(master, 65536)
                assert proc.poll() is None, ("unexpected exit", output)
            assert b"canned M0 submission accepted" not in output
            try:
                writer = os.open(os.path.join(config_dir, "credentials.json"), os.O_WRONLY | os.O_NONBLOCK)
            except OSError as error:
                import errno
                assert error.errno == errno.ENXIO, error
            else:
                os.close(writer)
                raise AssertionError("startup opened the credential FIFO")
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
            os.close(master)
            listener.close()


for ref in ({"stored": "p"}, {"env": "TEST_PROVIDER_KEY"}):
    check(sys.argv[1], False, ref)
    if sys.argv[2] == "demo":
        check(sys.argv[1], True, ref)
