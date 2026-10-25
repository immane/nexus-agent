"""Exercise incremental rendering with stdout and stderr on the same Unix PTY.

The screen reader implements the cursor/erase sequences used by Crossterm;
it is intentionally not a general-purpose terminal emulator.
"""

import fcntl
import os
import pty
import re
import select
import struct
import subprocess
import sys
import tempfile
import termios
import time


def check(binary, width, height, config):
    cells = [[" "] * width for _ in range(height)]
    x = y = 0

    def feed(data):
        nonlocal cells, x, y
        tokens = re.findall(
            r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\].*?(?:\x07|\x1b\\)|\x1b.|.",
            data.decode("utf-8"), re.S,
        )
        for token in tokens:
            if token.startswith("\x1b["):
                op = token[-1]
                ns = [int(v or "0") for v in token[2:-1].lstrip("?").split(";")]
                n = ns[0] or 1
                if op in "Hf":
                    y = min(height - 1, n - 1)
                    x = min(width - 1, ((ns[1] or 1) if len(ns) > 1 else 1) - 1)
                elif op == "J" and ns[0] in (2, 3):
                    cells = [[" "] * width for _ in range(height)]
                elif op == "K":
                    lo, hi = ((0, width) if ns[0] == 2 else
                              ((0, x + 1) if ns[0] == 1 else (x, width)))
                    cells[y][lo:hi] = [" "] * (hi - lo)
                elif op == "A": y = max(0, y - n)
                elif op == "B": y = min(height - 1, y + n)
                elif op == "C": x = min(width - 1, x + n)
                elif op == "D": x = max(0, x - n)
                elif op == "G": x = n - 1
            elif token.startswith("\x1b"):
                pass
            elif token == "\r":
                x = 0
            elif token == "\n":
                y += 1
                if y >= height:
                    cells.pop(0)
                    cells.append([" "] * width)
                    y = height - 1
            elif ord(token) >= 32:
                if x >= width:
                    x = 0
                    y += 1
                    if y >= height:
                        cells.pop(0)
                        cells.append([" "] * width)
                        y = height - 1
                cells[y][x] = token
                x += 1

    def row(n):
        return "".join(cells[n])

    master, slave = pty.openpty()
    try:
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        env = dict(os.environ, NEXUS_CONFIG=config)
        proc = subprocess.Popen([binary, "--demo"], stdin=slave, stdout=slave,
                                stderr=slave, env=env)
    finally:
        os.close(slave)

    def drain(seconds):
        out = b""
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            ready, _, _ = select.select([master], [], [], max(0, end - time.monotonic()))
            if ready:
                try:
                    out += os.read(master, 65536)
                except OSError:
                    break
        return out

    def step(keys, label):
        os.write(master, keys)
        out = drain(0.35)
        assert b"nexus-runtime:" not in out, (label, out)
        feed(out)
        if label == "help":
            assert "DESKTOP COMMANDS" in "\n".join(row(i) for i in range(height))
            return
        assert row(height - 4).startswith("┌ composer"), (label, row(height - 4))
        assert row(height - 4).endswith("┐"), (label, row(height - 4))
        assert row(height - 3)[0] == "│" and row(height - 3)[-1] == "│", label
        assert row(height - 2).startswith("└") and row(height - 2).endswith("┘"), label

    try:
        initial = drain(1)
        # Discard the pre-TUI banner on the primary screen.
        alternate = initial.index(b"\x1b[?1049h")
        feed(initial[alternate:])
        step(b"\x1b", "viewport")
        step(b":", "commands popup")
        assert ":▊" in row(height - 1), row(height - 1)
        assert "commands" in "\n".join(row(i) for i in range(height - 4))
        step(b"session new\r", "new session")
        assert "sess:s2" in row(0), row(0)
        assert "new conversation" in "\n".join(row(i) for i in range(height))
        step(b"\x1b", "viewport")
        step(b":session list\r", "list")
        step(b":session switch s1\r", "switch back")
        assert "sess:s1" in row(0), row(0)
        step(b":help\r", "help")
        step(b"\x1b", "close help")
        print(f"PASS: PTY {width}x{height}: commands/new/list/switch/help; borders intact")
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        os.close(master)


with tempfile.TemporaryDirectory(prefix="nexus-tui-pty-") as directory:
    for width, height in [(80, 24), (120, 32), (174, 40)]:
        check(sys.argv[1], width, height, os.path.join(directory, "absent-config.json"))
