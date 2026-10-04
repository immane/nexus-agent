#!/usr/bin/env python3
"""PTY-observed readiness and live-resource harness (Python 3 stdlib only).

Observes an unmodified target process from outside through a pseudo-terminal
and OS process facts. It does not instrument the target and does not read
target configuration.

Measured, in this harness's own vocabulary:

- observed-frame-ready: from spawn until the ANSI-filtered PTY output
  contains every required frame marker. Defaults are the M0-test TUI's
  fixed composer title ("composer (fixed)") and footer ("m0-test"); the
  header ("nexus-tui") is reported as supporting evidence but is not
  required. This is observed frame output, not proof that input handling
  is live; use --probe-input for an observational sentinel-key check.
- observed-key-in-output (optional): after observed-frame-ready, one
  sentinel ASCII key is written and its bytes are looked for in a bounded
  post-probe output window. Observational only: the key appearing in
  output does not prove the target consumed or acted on the input.
- idle CPU / RSS high-water: sampled only while the target stays alive
  after observed-frame-ready. RSS high-water comes from os.wait4 rusage on
  the directly waited child; live RSS sampling uses /proc on Linux and ps
  on macOS. Wakeup accounting is explicitly unsupported: no privileged
  tooling (no root, no powermetrics) is used.
- cancellation observation: only if the target is still alive after
  observed-frame-ready. A marker time is reported only when explicit
  post-cancel evidence appears: by default the case-insensitive terminal
  state markers "run cancelled" or "status: cancelled". Generic footer
  text such as "ctrl+c cancel" never counts. Custom --cancel-evidence
  literals or --cancel-evidence-regex patterns are caller-supplied and
  are reported as observational markers, not authoritative lifecycle
  proof. If the target already finished, or no marker appears, the result
  is "skipped"/"unconfirmed"/"timeout" and no latency is fabricated.

Honesty rules:
- warm cache only: this harness never runs sudo, docker, installs, or
  cache resets, and never claims cold-cache numbers.
- machine JSON evidence records source commit, Cargo.lock SHA-256, binary
  SHA-256, build profile, platform, uname, compiler, command, and
  user-supplied labels.
- capture memory is bounded; every run is bounded by timeouts, and the
  target's process group is killed and reaped on timeout.

Scope note: this is PTY-observed startup readiness plus live idle/RSS and
external cancellation observation. It is not the full P7 evidence set
(no streaming throughput, event-buffer high-water, binary-size budget, or
cold-cache claims).

Usage:
    python3 readiness.py target/release/nexus-tui --mode ready --runs 10
    python3 readiness.py target/release/nexus-tui --mode idle --idle-window 2
    python3 readiness.py target/release/nexus-tui --mode cancel
    python3 readiness.py /path/to/binary -- --target-arg

Without --json the JSON document goes to stdout and the human summary to
stderr; with --json PATH the document is written there and the human
summary goes to stdout. Exit codes: 0 harness completed (skips allowed),
1 at least one run failed (spawn error, ready timeout, overall timeout, or
unreaped target), 2 usage error.
"""

import argparse
import errno
import hashlib
import json
import math
import os
import platform
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone

try:
    import fcntl
    import termios
except ImportError:  # pragma: no cover - non-POSIX import still works
    fcntl = None
    termios = None

SCHEMA = "nexus.perf.readiness/1"
DEFAULT_REQUIRED = ("composer (fixed)", "m0-test")
DEFAULT_HEADER_MARKER = "nexus-tui"
DEFAULT_CANCEL_EVIDENCE = ("run cancelled", "status: cancelled")
ALT_SCREEN_SEQUENCE = "\x1b[?1049h"
FAILURE_OUTCOMES = ("spawn-error", "ready-timeout", "run-timeout", "wait-error")
POSIX_FORK = os.name == "posix" and hasattr(os, "fork") and hasattr(os, "wait4")
PIPE_READ_SIZE = 65536
MARKER_TAIL_BYTES = 65536


def percentile_nearest_rank(sorted_values, pct):
    """Nearest-rank percentile over an already-sorted list (local helper)."""
    if not sorted_values:
        return None
    if len(sorted_values) == 1:
        return float(sorted_values[0])
    rank = (pct / 100.0) * len(sorted_values)
    idx = min(max(math.ceil(rank) - 1, 0), len(sorted_values) - 1)
    return float(sorted_values[idx])


def summarize_ms(values_ms):
    """Local timing summary; intentionally not imported from startup.py."""
    ordered = sorted(float(value) for value in values_ms)
    if not ordered:
        return {"n": 0, "p50_ms": None, "p95_ms": None, "max_ms": None}
    return {
        "n": len(ordered),
        "p50_ms": percentile_nearest_rank(ordered, 50),
        "p95_ms": percentile_nearest_rank(ordered, 95),
        "max_ms": ordered[-1],
    }


def positive_seconds(value):
    """argparse type: reject zero, negative, NaN, and infinite timeouts."""
    try:
        seconds = float(value)
    except (TypeError, ValueError):
        raise argparse.ArgumentTypeError(f"invalid number: {value!r}")
    if not math.isfinite(seconds) or seconds <= 0:
        raise argparse.ArgumentTypeError("must be a positive finite number of seconds")
    return seconds


def positive_int(value):
    try:
        number = int(value)
    except (TypeError, ValueError):
        raise argparse.ArgumentTypeError(f"invalid integer: {value!r}")
    if number < 1:
        raise argparse.ArgumentTypeError("must be a positive integer")
    return number


def hex_bytes(value):
    """argparse type: even-length hex string such as '04' or '1b'."""
    text = value.strip().lower().replace(" ", "")
    if not text or len(text) % 2 or any(char not in "0123456789abcdef" for char in text):
        raise argparse.ArgumentTypeError("expected an even-length hex string like 04 or 03")
    return bytes.fromhex(text)


def label_pair(value):
    """argparse type: user-supplied KEY=VALUE evidence label."""
    key, separator, label = value.partition("=")
    key = key.strip()
    if not separator or not key:
        raise argparse.ArgumentTypeError("expected KEY=VALUE")
    return key, label


def sha256_file(path):
    """SHA-256 of a local file; never raises."""
    digest = hashlib.sha256()
    try:
        with open(path, "rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError:
        return "unknown"
    return digest.hexdigest()


class AnsiFilter:
    """Incremental ANSI escape filter: keeps visible text, drops escapes.

    Escape sequences may be split across PTY reads, so state is carried
    between feed() calls. UTF-8 bytes (>= 0x80) and tab/newline are kept;
    other C0 control bytes are dropped.
    """

    def __init__(self):
        self.state = "text"

    def feed(self, data):
        out = bytearray()
        for byte in data:
            if self.state == "text":
                if byte == 0x1B:
                    self.state = "esc"
                elif byte in (0x0A, 0x09) or 0x20 <= byte < 0x7F or byte >= 0x80:
                    out.append(byte)
            elif self.state == "esc":
                if byte == 0x5B:
                    self.state = "csi"
                elif byte == 0x5D:
                    self.state = "osc"
                elif byte in (0x28, 0x29, 0x2A, 0x2B):
                    self.state = "skip-one"
                else:
                    self.state = "text"
            elif self.state == "skip-one":
                self.state = "text"
            elif self.state == "csi":
                if 0x40 <= byte <= 0x7E:
                    self.state = "text"
            elif self.state == "osc":
                if byte == 0x07:
                    self.state = "text"
                elif byte == 0x1B:
                    self.state = "osc-esc"
            elif self.state == "osc-esc":
                if byte == 0x5C:
                    self.state = "text"
                elif byte != 0x1B:
                    self.state = "osc"
        return bytes(out)


class CaptureBuffer:
    """Bounded raw capture ring; counts total and dropped bytes."""

    def __init__(self, max_bytes):
        self.max_bytes = max(0, int(max_bytes))
        self.buffer = bytearray()
        self.total_bytes = 0
        self.dropped_bytes = 0

    def feed(self, data):
        self.total_bytes += len(data)
        if self.max_bytes == 0:
            self.dropped_bytes += len(data)
            return
        if len(data) >= self.max_bytes:
            self.dropped_bytes += len(self.buffer) + len(data) - self.max_bytes
            self.buffer[:] = data[-self.max_bytes:]
            return
        overflow = len(self.buffer) + len(data) - self.max_bytes
        if overflow > 0:
            del self.buffer[:overflow]
            self.dropped_bytes += overflow
        self.buffer.extend(data)


class MarkerTracker:
    """Bounded rolling-tail substring tracker for bytes markers."""

    def __init__(self, markers):
        self.markers = [str(marker) for marker in markers]
        self.seen = {marker: False for marker in self.markers}
        self.tail = bytearray()
        longest = max((len(marker.encode("utf-8")) for marker in self.markers), default=1)
        self.tail_limit = max(MARKER_TAIL_BYTES, longest * 4)

    def feed(self, visible):
        if not self.markers or not visible:
            return
        self.tail.extend(visible)
        if len(self.tail) > self.tail_limit:
            del self.tail[: len(self.tail) - self.tail_limit]
        haystack = bytes(self.tail)
        for marker in self.markers:
            if not self.seen[marker] and marker.encode("utf-8") in haystack:
                self.seen[marker] = True

    @property
    def complete(self):
        return all(self.seen.values())

    def missing(self):
        return [marker for marker in self.markers if not self.seen[marker]]


class EvidenceTracker:
    """Any-of literal/regex marker tracker over a bounded rolling tail.

    Used only for cancel evidence, where markers are alternatives rather
    than required together. Literal matching is case-insensitive so a
    terminal state line is recognized regardless of case. Regex patterns
    are caller-supplied and therefore observational, never authoritative
    lifecycle proof.
    """

    def __init__(self, literals=(), regexes=(), case_insensitive=True):
        self.case_insensitive = case_insensitive
        self.literals = list(literals)
        self.regexes = list(regexes)
        self._literal_needles = [
            (marker, (marker.lower() if case_insensitive else marker).encode("utf-8"))
            for marker in self.literals
        ]
        flags = re.IGNORECASE if case_insensitive else 0
        self._compiled = [
            (pattern, re.compile(pattern.encode("utf-8"), flags))
            for pattern in self.regexes
        ]
        self.seen = {("literal", marker): False for marker in self.literals}
        self.seen.update({("regex", pattern): False for pattern in self.regexes})
        self.tail = bytearray()
        longest = max([len(needle) for _, needle in self._literal_needles] + [1])
        self.tail_limit = max(MARKER_TAIL_BYTES, longest * 4)

    def feed(self, visible):
        if not self.seen or not visible:
            return
        self.tail.extend(visible.lower() if self.case_insensitive else visible)
        if len(self.tail) > self.tail_limit:
            del self.tail[: len(self.tail) - self.tail_limit]
        haystack = bytes(self.tail)
        for marker, needle in self._literal_needles:
            if not self.seen[("literal", marker)] and needle in haystack:
                self.seen[("literal", marker)] = True
        for pattern, compiled in self._compiled:
            if not self.seen[("regex", pattern)] and compiled.search(haystack):
                self.seen[("regex", pattern)] = True

    @property
    def complete(self):
        return any(self.seen.values())

    def missing(self):
        return [
            marker for (kind, marker), found in self.seen.items() if not found
        ]


def decode_wait_status(status):
    if os.WIFEXITED(status):
        return os.WEXITSTATUS(status), None
    if os.WIFSIGNALED(status):
        return None, os.WTERMSIG(status)
    return None, None


class PtyTarget:
    """Directly forked child on a PTY; reaped with os.wait4 (target RSS)."""

    def __init__(self, pid, master, started_ns):
        self.pid = pid
        self.master = master
        self.started_ns = started_ns
        self.exit_code = None
        self.signal = None
        self.ru_maxrss = None
        self.exited_ns = None
        self.reaped = False
        self.read_error = None

    def elapsed_ms(self):
        return (time.monotonic_ns() - self.started_ns) / 1e6

    def write(self, data):
        try:
            os.write(self.master, data)
            return True
        except OSError:
            return False

    def read_chunk(self, timeout_s):
        """Returns (data, eof); eof means the PTY reported end-of-stream."""
        if self.master < 0:
            return b"", True
        try:
            readable, _, _ = select.select([self.master], [], [], max(0.0, timeout_s))
        except InterruptedError:
            return b"", False
        if not readable:
            return b"", False
        try:
            data = os.read(self.master, PIPE_READ_SIZE)
        except BlockingIOError:
            return b"", False
        except OSError as exc:
            if exc.errno == errno.EIO:
                return b"", True
            self.read_error = f"{type(exc).__name__}: {exc}"
            return b"", True
        if not data:
            return b"", True
        return data, False

    def check_exit(self):
        """Non-blocking wait4; records exit status, rusage, and exit time."""
        if self.reaped:
            return True
        try:
            pid, status, usage = os.wait4(self.pid, os.WNOHANG)
        except ChildProcessError:
            self.reaped = True
            return True
        except OSError as exc:
            self.read_error = f"wait4: {type(exc).__name__}: {exc}"
            self.reaped = True
            return True
        if pid == 0:
            return False
        self.exit_code, self.signal = decode_wait_status(status)
        self.ru_maxrss = getattr(usage, "ru_maxrss", None)
        self.reaped = True
        self.exited_ns = time.monotonic_ns()
        return True

    def close(self):
        if self.master >= 0:
            try:
                os.close(self.master)
            except OSError:
                pass
            self.master = -1


def _login_tty(slave):
    if hasattr(os, "login_tty"):
        os.login_tty(slave)
        return
    os.setsid()
    if fcntl is not None and termios is not None:
        try:
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
        except OSError:
            pass
    os.dup2(slave, 0)
    os.dup2(slave, 1)
    os.dup2(slave, 2)
    if slave > 2:
        os.close(slave)


def spawn_pty(argv, cols=120, rows=40):
    """Fork+exec argv on a fresh PTY; the child becomes a session leader."""
    if not POSIX_FORK or fcntl is None or termios is None:
        raise RuntimeError("PTY spawning requires POSIX fork/openpty support")
    if not argv:
        raise ValueError("empty argv")
    master, slave = os.openpty()
    try:
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    except OSError:
        pass
    started_ns = time.monotonic_ns()
    pid = os.fork()
    if pid == 0:
        try:
            os.close(master)
            _login_tty(slave)
            os.execvpe(argv[0], argv, os.environ.copy())
        except BaseException:
            os._exit(127)
    os.close(slave)
    os.set_blocking(master, False)
    return PtyTarget(pid=pid, master=master, started_ns=started_ns)


def _signal_group(pid, signum):
    try:
        os.killpg(pid, signum)
        return
    except OSError:
        pass
    try:
        os.kill(pid, signum)
    except OSError:
        pass


def _parse_ps_time(text):
    if not text:
        raise ValueError("empty ps time")
    days = 0
    if "-" in text:
        days_text, text = text.split("-", 1)
        days = int(days_text)
    parts = text.split(":")
    if len(parts) == 3:
        hours, minutes, seconds = parts
    elif len(parts) == 2:
        hours, minutes, seconds = "0", parts[0], parts[1]
    else:
        raise ValueError(f"unrecognized ps time: {text!r}")
    return days * 86400 + int(hours) * 3600 + int(minutes) * 60 + float(seconds)


def _linux_status(pid, key):
    try:
        with open(f"/proc/{pid}/status", encoding="utf-8", errors="replace") as handle:
            for line in handle:
                if line.startswith(key):
                    return line.split(":", 1)[1].strip()
    except OSError:
        return None
    return None


def _linux_context_switches(pid):
    voluntary = _linux_status(pid, "voluntary_ctxt_switches")
    nonvoluntary = _linux_status(pid, "nonvoluntary_ctxt_switches")
    if voluntary is None and nonvoluntary is None:
        return None
    try:
        return {
            "voluntary": int(voluntary) if voluntary is not None else None,
            "nonvoluntary": int(nonvoluntary) if nonvoluntary is not None else None,
        }
    except ValueError:
        return None


def sample_cpu(pid):
    """Cumulative CPU seconds for a live pid; platform-labelled."""
    if sys.platform.startswith("linux"):
        try:
            with open(f"/proc/{pid}/stat", "rb") as handle:
                raw = handle.read().decode("utf-8", "replace")
            rest = raw[raw.rfind(")") + 2:].split()
            ticks = int(rest[11]) + int(rest[12])
            hertz = os.sysconf("SC_CLK_TCK")
            return {
                "available": True,
                "cpu_seconds": ticks / hertz,
                "method": "/proc/<pid>/stat utime+stime",
                "context_switches": _linux_context_switches(pid),
            }
        except (OSError, ValueError, IndexError):
            return {
                "available": False,
                "cpu_seconds": None,
                "method": "/proc/<pid>/stat",
                "context_switches": None,
            }
    if sys.platform == "darwin":
        try:
            completed = subprocess.run(
                ["ps", "-o", "time=", "-p", str(pid)],
                capture_output=True,
                text=True,
                timeout=5,
            )
            return {
                "available": completed.returncode == 0,
                "cpu_seconds": _parse_ps_time(completed.stdout.strip()),
                "method": "ps -o time= (cumulative)",
                "context_switches": None,
            }
        except (OSError, subprocess.SubprocessError, ValueError):
            return {
                "available": False,
                "cpu_seconds": None,
                "method": "ps -o time=",
                "context_switches": None,
            }
    return {
        "available": False,
        "cpu_seconds": None,
        "method": "unsupported platform",
        "context_switches": None,
    }


def sample_rss(pid):
    """Live RSS and, where available, peak RSS (bytes)."""
    if sys.platform.startswith("linux"):
        rss_kb = _linux_status(pid, "VmRSS")
        peak_kb = _linux_status(pid, "VmHWM")
        try:
            rss = int(rss_kb.split()[0]) * 1024 if rss_kb else None
            peak = int(peak_kb.split()[0]) * 1024 if peak_kb else None
        except (ValueError, IndexError):
            rss = peak = None
        return {
            "rss_bytes": rss,
            "peak_rss_bytes": peak,
            "method": "/proc/<pid>/status VmRSS/VmHWM",
        }
    if sys.platform == "darwin":
        try:
            completed = subprocess.run(
                ["ps", "-o", "rss=", "-p", str(pid)],
                capture_output=True,
                text=True,
                timeout=5,
            )
            rss_kb = int(completed.stdout.strip())
            return {
                "rss_bytes": rss_kb * 1024,
                "peak_rss_bytes": None,
                "method": "ps -o rss= (sampled)",
            }
        except (OSError, subprocess.SubprocessError, ValueError):
            return {"rss_bytes": None, "peak_rss_bytes": None, "method": "ps -o rss="}
    return {"rss_bytes": None, "peak_rss_bytes": None, "method": "unsupported platform"}


def rss_unit():
    if sys.platform == "darwin":
        return "bytes (macOS ru_maxrss)"
    if sys.platform.startswith("linux"):
        return "kilobytes (Linux ru_maxrss)"
    return "platform-specific ru_maxrss units"


def collect_compiler():
    """Best-effort rustc/cargo identity; never installs or downloads."""
    info = {"rustc": None, "rustc_commit": None, "cargo": None}
    rustc = shutil.which("rustc")
    if rustc:
        try:
            completed = subprocess.run(
                [rustc, "-Vv"], capture_output=True, text=True, timeout=5
            )
            if completed.returncode == 0:
                lines = completed.stdout.splitlines()
                info["rustc"] = lines[0] if lines else None
                for line in lines:
                    if line.startswith("commit-hash:"):
                        info["rustc_commit"] = line.split(":", 1)[1].strip()
        except (OSError, subprocess.SubprocessError):
            pass
    cargo = shutil.which("cargo")
    if cargo:
        try:
            completed = subprocess.run(
                [cargo, "-V"], capture_output=True, text=True, timeout=5
            )
            if completed.returncode == 0:
                info["cargo"] = completed.stdout.strip()
        except (OSError, subprocess.SubprocessError):
            pass
    return info


def collect_provenance(lockfile=None):
    """Best-effort source commit and Cargo.lock hash; bounded, never fatal."""
    info = {
        "source_commit": None,
        "source_dirty": None,
        "lockfile": None,
        "lockfile_sha256": None,
    }
    try:
        head = subprocess.run(
            ["git", "rev-parse", "HEAD"], capture_output=True, text=True, timeout=5
        )
        if head.returncode == 0:
            info["source_commit"] = head.stdout.strip()
            status = subprocess.run(
                ["git", "status", "--porcelain"], capture_output=True, text=True, timeout=5
            )
            if status.returncode == 0:
                info["source_dirty"] = bool(status.stdout.strip())
    except (OSError, subprocess.SubprocessError):
        pass
    candidates = []
    if lockfile:
        candidates.append(lockfile)
    candidates.append(os.path.join(os.getcwd(), "Cargo.lock"))
    here = os.path.dirname(os.path.abspath(__file__))
    candidates.append(os.path.normpath(os.path.join(here, "..", "..", "Cargo.lock")))
    for candidate in candidates:
        if candidate and os.path.isfile(candidate):
            info["lockfile"] = os.path.abspath(candidate)
            info["lockfile_sha256"] = sha256_file(candidate)
            break
    return info


def collect_methodology(binary, command, build_profile, labels, required, cols, rows):
    """Machine evidence header for one harness invocation."""
    info = {
        "schema": SCHEMA,
        "timestamp_utc": datetime.now(timezone.utc).isoformat(),
        "binary": binary,
        "binary_sha256": sha256_file(binary),
        "build_profile": build_profile,
        "command": list(command),
        "user_supplied": dict(labels),
        "platform": {
            "system": platform.system(),
            "release": platform.release(),
            "version": platform.version(),
            "machine": platform.machine(),
            "processor": platform.processor() or "unknown",
        },
        "uname": dict(
            zip(
                ("sysname", "nodename", "release", "version", "machine"),
                platform.uname(),
            )
        ),
        "python": platform.python_version(),
        "compiler": collect_compiler(),
        "required_markers": list(required),
        "terminal": {"cols": cols, "rows": rows},
        "observation": "external PTY output plus OS process facts; no target instrumentation",
        "cache_state": "warm",
        "cache_note": (
            "no cache reset attempted; this harness never runs sudo, docker, "
            "package installs, or drop_caches"
        ),
        "cold_measurements": "unsupported by design in this harness",
    }
    try:
        info["binary_bytes"] = os.path.getsize(binary)
    except OSError:
        info["binary_bytes"] = None
    info.update(collect_provenance())
    return info


@dataclass
class RunConfig:
    mode: str = "ready"
    timeout: float = 30.0
    ready_timeout: float = 10.0
    quit_timeout: float = 3.0
    cancel_timeout: float = 5.0
    kill_grace: float = 1.0
    idle_window: float = 2.0
    idle_interval: float = 0.25
    probe_input: bool = False
    probe_char: str = "z"
    probe_timeout: float = 2.0
    cancel_settle: float = 0.15
    cancel_evidence: tuple = DEFAULT_CANCEL_EVIDENCE
    cancel_evidence_regex: tuple = ()
    cols: int = 120
    rows: int = 40
    max_capture_bytes: int = 2_000_000
    required: tuple = DEFAULT_REQUIRED
    header_marker: str = DEFAULT_HEADER_MARKER
    quit_keys: bytes = b"\x04"
    cancel_keys: bytes = b"\x03"


def _context_switch_delta(first, second):
    if not first or not second:
        return None
    if first.get("voluntary") is None or second.get("voluntary") is None:
        return None
    return {
        "voluntary": second["voluntary"] - first["voluntary"],
        "nonvoluntary": second["nonvoluntary"] - first["nonvoluntary"],
    }


def _idle_supported_note():
    return {
        "status": "unsupported",
        "reason": (
            "unprivileged wakeup counting is not implemented; this harness never "
            "uses root or privileged tooling"
        ),
    }


def run_measurement(cfg, argv, run_index=0):
    """Run one measured target session; returns a JSON-serializable dict."""
    result = {
        "run_index": run_index,
        "argv": list(argv),
        "spawn": {"status": "ok", "error": None},
        "ready": None,
        "input_probe": None,
        "idle": None,
        "cancel": None,
        "exit": None,
        "resources": None,
        "capture": None,
        "outcome": "ok",
    }
    try:
        target = spawn_pty(argv, cols=cfg.cols, rows=cfg.rows)
    except (OSError, RuntimeError, ValueError) as exc:
        result["spawn"] = {"status": "error", "error": f"{type(exc).__name__}: {exc}"}
        result["outcome"] = "spawn-error"
        return result
    result["pid"] = target.pid

    capture = CaptureBuffer(cfg.max_capture_bytes)
    ansi = AnsiFilter()
    tracker = MarkerTracker(cfg.required)
    header_tracker = MarkerTracker((cfg.header_marker,)) if cfg.header_marker else None
    alt_tracker = MarkerTracker((ALT_SCREEN_SEQUENCE,))
    echo_tracker = None
    cancel_evidence_tracker = None
    ready_ms = None
    first_visible_ms = None
    echo_ms = None
    cancel_evidence_ms = None

    def feed(data):
        nonlocal ready_ms, first_visible_ms, echo_ms, cancel_evidence_ms
        capture.feed(data)
        alt_tracker.feed(data)
        visible = ansi.feed(data)
        if not visible:
            return
        tracker.feed(visible)
        if header_tracker is not None:
            header_tracker.feed(visible)
        if echo_tracker is not None:
            echo_tracker.feed(visible)
        if cancel_evidence_tracker is not None:
            cancel_evidence_tracker.feed(visible)
        now_ms = target.elapsed_ms()
        if first_visible_ms is None:
            first_visible_ms = now_ms
        if ready_ms is None and tracker.complete:
            ready_ms = now_ms
        if echo_tracker is not None and echo_ms is None and echo_tracker.complete:
            echo_ms = now_ms
        if (
            cancel_evidence_tracker is not None
            and cancel_evidence_ms is None
            and cancel_evidence_tracker.complete
        ):
            cancel_evidence_ms = now_ms

    def drain(timeout_s):
        data, _ = target.read_chunk(timeout_s)
        if data:
            feed(data)

    def pump(deadline_ns, done=None):
        """Read until done()/deadline/exit; returns done|deadline|exited."""
        while True:
            if done is not None and done():
                return "done"
            if target.check_exit():
                drain(0.05)
                return "exited"
            now_ns = time.monotonic_ns()
            if now_ns >= deadline_ns:
                return "deadline"
            timeout_s = min((deadline_ns - now_ns) / 1e9, 0.05)
            data, eof = target.read_chunk(timeout_s)
            if data:
                feed(data)
            if eof and target.check_exit():
                return "exited"

    run_deadline = target.started_ns + int(cfg.timeout * 1e9)
    overall_hit = False
    try:
        ready_deadline = min(run_deadline, target.started_ns + int(cfg.ready_timeout * 1e9))
        pump(ready_deadline, done=lambda: tracker.complete)
        if ready_ms is not None:
            ready_status = "observed-frame-ready"
        elif target.reaped:
            ready_status = "exited-before-ready"
        else:
            ready_status = "ready-timeout"
        result["ready"] = {
            "status": ready_status,
            "ms": ready_ms,
            "first_visible_ms": first_visible_ms,
            "required": list(cfg.required),
            "missing": tracker.missing(),
            "header_seen": header_tracker.complete if header_tracker is not None else None,
            "alt_screen_seen": alt_tracker.complete,
            "definition": "spawn to ANSI-filtered PTY output containing all required markers",
            "note": "observed frame output, not proof that input handling is live",
        }

        if ready_ms is not None and cfg.probe_input:
            probe_bounded_ms = cfg.probe_timeout * 1000.0
            if target.check_exit():
                result["input_probe"] = {
                    "status": "exited-before-probe",
                    "ms": None,
                    "bounded_ms": probe_bounded_ms,
                    "char": cfg.probe_char,
                    "wrote_keys": False,
                    "interpretation": (
                        "observational: target exited before the sentinel key "
                        "was written"
                    ),
                }
            else:
                echo_tracker = MarkerTracker((cfg.probe_char,))
                probe_start_ns = time.monotonic_ns()
                wrote = target.write(cfg.probe_char.encode("utf-8"))
                probe_deadline = min(
                    run_deadline, probe_start_ns + int(cfg.probe_timeout * 1e9)
                )
                if wrote:
                    pump(probe_deadline, done=lambda: echo_tracker.complete)
                if echo_tracker.complete and echo_ms is not None:
                    result["input_probe"] = {
                        "status": "observed-key-in-output",
                        "ms": (echo_ms - (probe_start_ns - target.started_ns) / 1e6),
                        "bounded_ms": probe_bounded_ms,
                        "char": cfg.probe_char,
                        "wrote_keys": wrote,
                        "definition": (
                            "key write to first appearance of the sentinel key in "
                            "bounded post-probe output"
                        ),
                        "interpretation": (
                            "observational: the key bytes appeared in post-probe "
                            "output; this does not prove the application consumed "
                            "or acted on the input"
                        ),
                    }
                elif target.reaped:
                    result["input_probe"] = {
                        "status": "exited-before-key",
                        "ms": None,
                        "bounded_ms": probe_bounded_ms,
                        "char": cfg.probe_char,
                        "wrote_keys": wrote,
                        "interpretation": (
                            "observational: target exited before the sentinel key "
                            "appeared in post-probe output"
                        ),
                    }
                else:
                    result["input_probe"] = {
                        "status": "key-not-observed",
                        "ms": None,
                        "bounded_ms": probe_bounded_ms,
                        "char": cfg.probe_char,
                        "wrote_keys": wrote,
                        "interpretation": (
                            "observational: no sentinel key bytes in bounded "
                            "post-probe output"
                        ),
                    }

        if ready_ms is not None and cfg.mode == "idle":
            wall_start_ns = time.monotonic_ns()
            cpu_first = sample_cpu(target.pid)
            rss_samples = []
            peak_sampled = None
            window_end_ns = min(
                run_deadline, wall_start_ns + int(cfg.idle_window * 1e9)
            )
            exited_during_window = False
            while True:
                if target.check_exit():
                    exited_during_window = True
                    break
                now_ns = time.monotonic_ns()
                if now_ns >= window_end_ns:
                    break
                step_deadline = min(window_end_ns, now_ns + int(cfg.idle_interval * 1e9))
                if pump(step_deadline) == "exited":
                    exited_during_window = True
                    break
                rss = sample_rss(target.pid)
                if rss["rss_bytes"] is not None:
                    rss_samples.append(rss["rss_bytes"])
                if rss["peak_rss_bytes"] is not None:
                    peak_sampled = max(peak_sampled or 0, rss["peak_rss_bytes"])
            wall_end_ns = time.monotonic_ns()
            cpu_second = sample_cpu(target.pid)
            window_s = (wall_end_ns - wall_start_ns) / 1e9
            cpu_seconds = None
            if cpu_first.get("available") and cpu_second.get("available"):
                cpu_seconds = cpu_second["cpu_seconds"] - cpu_first["cpu_seconds"]
            idle_percent = None
            if cpu_seconds is not None and window_s > 0:
                idle_percent = (cpu_seconds / window_s) * 100.0
            short_window = exited_during_window or window_s < cfg.idle_window * 0.9
            result["idle"] = {
                "status": "short" if short_window else "observed",
                "window_s": window_s,
                "exited_during_window": exited_during_window,
                "cpu_seconds": cpu_seconds,
                "cpu_method": cpu_second.get("method"),
                "idle_cpu_percent": idle_percent,
                "rss_samples": len(rss_samples),
                "rss_peak_sampled_bytes": peak_sampled,
                "context_switches": _context_switch_delta(cpu_first, cpu_second),
                "wakeups": _idle_supported_note(),
            }

        if ready_ms is not None and cfg.mode == "cancel":
            if cfg.cancel_evidence_regex and not cfg.cancel_evidence:
                evidence_source = "user-supplied-regex"
            elif cfg.cancel_evidence_regex:
                evidence_source = "user-supplied-mixed"
            elif tuple(cfg.cancel_evidence) != DEFAULT_CANCEL_EVIDENCE:
                evidence_source = "user-supplied"
            else:
                evidence_source = "default"
            # Stable schema: every key is present for every status, with
            # None where the observation does not apply.
            cancel_common = {
                "reason": None,
                "cancel_marker_observed_ms": None,
                "cancel_to_exit_ms": None,
                "wrote_keys": None,
                "evidence_source": evidence_source,
                "evidence_literals": list(cfg.cancel_evidence),
                "evidence_regexes": list(cfg.cancel_evidence_regex),
                "missing_evidence": [],
                "exited_within_window": None,
                "interpretation": None,
                "latency_claim": None,
                "confound": None,
            }
            # Settle briefly so a target that was already finishing does so
            # before a cancel opportunity is claimed.
            settle_deadline = min(
                run_deadline, time.monotonic_ns() + int(cfg.cancel_settle * 1e9)
            )
            pump(settle_deadline)
            if target.check_exit():
                result["cancel"] = {
                    **cancel_common,
                    "status": "skipped",
                    "reason": "target finished before cancel was sent",
                    "exited_within_window": True,
                }
            else:
                cancel_evidence_tracker = EvidenceTracker(
                    literals=cfg.cancel_evidence,
                    regexes=cfg.cancel_evidence_regex,
                )
                cancel_start_ns = time.monotonic_ns()
                wrote = target.write(cfg.cancel_keys)
                cancel_deadline = min(
                    run_deadline, cancel_start_ns + int(cfg.cancel_timeout * 1e9)
                )
                if wrote:
                    pump(cancel_deadline, done=lambda: cancel_evidence_tracker.complete)
                marker_observed = (
                    cancel_evidence_tracker.complete and cancel_evidence_ms is not None
                )
                cancel_fields = {
                    **cancel_common,
                    "wrote_keys": wrote,
                    "missing_evidence": cancel_evidence_tracker.missing(),
                    "exited_within_window": target.reaped,
                }
                if marker_observed:
                    marker_ms = (
                        cancel_evidence_ms - (cancel_start_ns - target.started_ns) / 1e6
                    )
                    if not target.check_exit():
                        pump(cancel_deadline)
                    result["cancel"] = {
                        **cancel_fields,
                        "status": "observed",
                        "cancel_marker_observed_ms": marker_ms,
                        "cancel_to_exit_ms": (
                            (target.exited_ns - cancel_start_ns) / 1e6
                            if target.reaped
                            else None
                        ),
                        "exited_within_window": target.reaped,
                        "interpretation": (
                            "observational: the marker appeared in bounded post-cancel "
                            "output; this is not authoritative cancellation lifecycle "
                            "proof and no runtime-internal latency is claimed"
                        ),
                        "latency_claim": "observational-marker-only",
                        "confound": (
                            "external observation cannot distinguish a cancel-induced "
                            "exit from a concurrent normal completion"
                        ),
                    }
                elif target.reaped:
                    result["cancel"] = {
                        **cancel_fields,
                        "status": "unconfirmed",
                        "reason": (
                            "target exited without observed cancel evidence; "
                            "no latency reported"
                        ),
                    }
                else:
                    result["cancel"] = {
                        **cancel_fields,
                        "status": "unconfirmed",
                        "reason": (
                            f"no cancel evidence within {cfg.cancel_timeout:g}s; "
                            "target still alive when the window expired "
                            "(terminated during cleanup)"
                        ),
                    }

        if time.monotonic_ns() >= run_deadline and not target.reaped:
            overall_hit = True
    finally:
        cleanup = terminate_target(target, cfg, drain)
        drain(0.05)
        if not target.reaped:
            _signal_group(target.pid, signal.SIGKILL)
            reap_deadline = time.monotonic() + cfg.kill_grace
            while time.monotonic() < reap_deadline and not target.reaped:
                target.check_exit()
                time.sleep(0.02)
        result["exit"] = {
            "status": "exited" if target.reaped else "unreaped",
            "exit_code": target.exit_code,
            "signal": target.signal,
            "elapsed_ms": target.elapsed_ms(),
            "cleanup": cleanup,
            "read_error": target.read_error,
        }
        result["resources"] = {
            "ru_maxrss": target.ru_maxrss,
            "ru_maxrss_unit": rss_unit(),
            "method": "os.wait4 rusage of the directly waited child",
        }
        result["capture"] = {
            "retained_bytes": len(capture.buffer),
            "total_bytes": capture.total_bytes,
            "dropped_bytes": capture.dropped_bytes,
            "max_capture_bytes": capture.max_bytes,
            "alt_screen_seen": alt_tracker.complete,
        }
        target.close()

    if result["ready"] is not None and result["ready"]["status"] == "ready-timeout":
        result["outcome"] = "ready-timeout"
    elif overall_hit:
        result["outcome"] = "run-timeout"
    elif not target.reaped:
        result["outcome"] = "wait-error"
    else:
        result["outcome"] = "ok"
    return result


def terminate_target(target, cfg, drain):
    """Quit keys, then SIGTERM, then SIGKILL; always bounded."""
    cleanup = {"method": "already-exited", "escalated": False, "reaped": target.reaped}
    if target.check_exit():
        cleanup["reaped"] = True
        return cleanup
    target.write(cfg.quit_keys)
    cleanup["method"] = "quit-keys"
    deadline = time.monotonic() + cfg.quit_timeout
    while time.monotonic() < deadline:
        if target.check_exit():
            cleanup["reaped"] = True
            return cleanup
        drain(0.05)
    if not target.check_exit():
        cleanup["escalated"] = True
        cleanup["method"] = "sigterm"
        _signal_group(target.pid, signal.SIGTERM)
        deadline = time.monotonic() + cfg.kill_grace
        while time.monotonic() < deadline:
            if target.check_exit():
                cleanup["reaped"] = True
                return cleanup
            drain(0.05)
    if not target.check_exit():
        cleanup["method"] = "sigkill"
        _signal_group(target.pid, signal.SIGKILL)
        deadline = time.monotonic() + cfg.kill_grace
        while time.monotonic() < deadline:
            if target.check_exit():
                cleanup["reaped"] = True
                return cleanup
            drain(0.05)
    cleanup["reaped"] = target.reaped
    return cleanup


def summarize_runs(cfg, runs):
    """Aggregate one invocation's runs; counts and summaries only."""
    ready_ms = [
        run["ready"]["ms"]
        for run in runs
        if run["ready"] is not None
        and run["ready"]["status"] == "observed-frame-ready"
        and run["ready"]["ms"] is not None
    ]
    outcomes = {}
    for run in runs:
        outcomes[run["outcome"]] = outcomes.get(run["outcome"], 0) + 1
    summary = {
        "runs": len(runs),
        "outcomes": outcomes,
        "ready_ms": summarize_ms(ready_ms),
        "cleanup_escalations": sum(
            1
            for run in runs
            if run["exit"] is not None and run["exit"]["cleanup"]["escalated"]
        ),
        "scope_note": (
            "PTY-observed startup readiness plus live idle/RSS and external "
            "cancellation observation only; not the full P7 evidence set"
        ),
    }
    if cfg.mode == "idle":
        summary["idle"] = [
            {
                "run_index": run["run_index"],
                "status": run["idle"]["status"],
                "idle_cpu_percent": run["idle"]["idle_cpu_percent"],
                "window_s": run["idle"]["window_s"],
            }
            for run in runs
            if run["idle"] is not None
        ]
    if cfg.mode == "cancel":
        summary["cancel"] = {
            "observed": sum(
                1
                for run in runs
                if run["cancel"] is not None and run["cancel"]["status"] == "observed"
            ),
            "observed_marker_ms": [
                run["cancel"]["cancel_marker_observed_ms"]
                for run in runs
                if run["cancel"] is not None
                and run["cancel"]["status"] == "observed"
                and run["cancel"]["cancel_marker_observed_ms"] is not None
            ],
            "observed_to_exit_ms": [
                run["cancel"]["cancel_to_exit_ms"]
                for run in runs
                if run["cancel"] is not None
                and run["cancel"]["status"] == "observed"
                and run["cancel"]["cancel_to_exit_ms"] is not None
            ],
            "skipped": sum(
                1
                for run in runs
                if run["cancel"] is not None and run["cancel"]["status"] == "skipped"
            ),
            "unconfirmed": sum(
                1
                for run in runs
                if run["cancel"] is not None and run["cancel"]["status"] == "unconfirmed"
            ),
        }
    return summary


def build_parser():
    parser = argparse.ArgumentParser(
        description=(
            "Observe PTY frame readiness, idle CPU/RSS, and external cancellation "
            "for an unmodified target (stdlib only; warm cache; no privileged actions)."
        )
    )
    parser.add_argument("binary", help="Path to the target binary (or interpreter).")
    parser.add_argument(
        "--mode",
        choices=["ready", "idle", "cancel"],
        default="ready",
        help="ready: frame readiness only; idle: live CPU/RSS window; cancel: external cancel observation.",
    )
    parser.add_argument(
        "--runs",
        type=positive_int,
        default=None,
        help="Runs per invocation (default: 5 for ready, 1 for idle/cancel).",
    )
    parser.add_argument(
        "--timeout",
        type=positive_seconds,
        default=30.0,
        help="Overall per-run bound in seconds (default: 30).",
    )
    parser.add_argument(
        "--ready-timeout",
        type=positive_seconds,
        default=10.0,
        help="Observed-frame-ready bound in seconds (default: 10).",
    )
    parser.add_argument(
        "--quit-timeout",
        type=positive_seconds,
        default=3.0,
        help="Bound after quit keys before SIGTERM (default: 3).",
    )
    parser.add_argument(
        "--cancel-timeout",
        type=positive_seconds,
        default=5.0,
        help="Bound after the cancel key before the target is killed (default: 5).",
    )
    parser.add_argument(
        "--kill-grace",
        type=positive_seconds,
        default=1.0,
        help="Bound between SIGTERM/SIGKILL and the reap check (default: 1).",
    )
    parser.add_argument(
        "--idle-window",
        type=positive_seconds,
        default=2.0,
        help="Idle observation window after readiness, in seconds (default: 2).",
    )
    parser.add_argument(
        "--idle-interval",
        type=positive_seconds,
        default=0.25,
        help="Idle CPU/RSS sampling interval in seconds (default: 0.25).",
    )
    parser.add_argument(
        "--probe-input",
        action="store_true",
        help="After readiness, write one key and observe its echo (ready mode only).",
    )
    parser.add_argument("--probe-char", default="z", help="Key to write for --probe-input.")
    parser.add_argument(
        "--probe-timeout",
        type=positive_seconds,
        default=2.0,
        help="Echo bound after the probe key (default: 2).",
    )
    parser.add_argument("--cols", type=positive_int, default=120, help="PTY columns.")
    parser.add_argument("--rows", type=positive_int, default=40, help="PTY rows.")
    parser.add_argument(
        "--max-capture-bytes",
        type=positive_int,
        default=2_000_000,
        help="Bounded raw PTY capture ring size in bytes (default: 2,000,000).",
    )
    parser.add_argument(
        "--require",
        action="append",
        metavar="TEXT",
        help=(
            "Required frame marker; repeatable. Overrides the defaults "
            f"{list(DEFAULT_REQUIRED)}."
        ),
    )
    parser.add_argument(
        "--header-marker",
        default=DEFAULT_HEADER_MARKER,
        help=f"Header marker reported as supporting evidence (default: {DEFAULT_HEADER_MARKER!r}; empty disables).",
    )
    parser.add_argument(
        "--quit-hex",
        type=hex_bytes,
        default=b"\x04",
        help="Hex key bytes sent to quit (default: 04, Ctrl+D).",
    )
    parser.add_argument(
        "--cancel-hex",
        type=hex_bytes,
        default=b"\x03",
        help="Hex key bytes sent for cancellation (default: 03, Ctrl+C).",
    )
    parser.add_argument(
        "--cancel-settle",
        type=positive_seconds,
        default=0.15,
        help=(
            "Settle wait after readiness before the cancel opportunity is "
            "checked (default: 0.15)."
        ),
    )
    parser.add_argument(
        "--cancel-evidence",
        action="append",
        metavar="TEXT",
        help=(
            "Case-insensitive post-cancel literal evidence marker; repeatable "
            "and any-of. Overrides the explicit terminal-state defaults "
            f"{list(DEFAULT_CANCEL_EVIDENCE)}. Generic footer text such as "
            "'ctrl+c cancel' must not be used as evidence."
        ),
    )
    parser.add_argument(
        "--cancel-evidence-regex",
        action="append",
        metavar="PATTERN",
        help=(
            "Caller-supplied case-insensitive regex evidence; repeatable and "
            "any-of. When given without --cancel-evidence, only these patterns "
            "are tracked and are reported as user-supplied observational markers."
        ),
    )
    parser.add_argument(
        "--build-profile",
        default="unknown/external binary",
        help="Build-profile label recorded in methodology (the script cannot detect it).",
    )
    parser.add_argument(
        "--label",
        action="append",
        type=label_pair,
        default=[],
        metavar="KEY=VALUE",
        help="User-supplied evidence label; repeatable.",
    )
    parser.add_argument(
        "--lockfile",
        default=None,
        help="Explicit Cargo.lock path for the provenance hash.",
    )
    parser.add_argument(
        "--json",
        metavar="PATH",
        default=None,
        help="Write the JSON document to PATH; without it the JSON goes to stdout.",
    )
    return parser


def main(argv=None):
    raw = list(sys.argv[1:] if argv is None else argv)
    if "--" in raw:
        cut = raw.index("--")
        own, binary_args = raw[:cut], raw[cut + 1:]
    else:
        own, binary_args = raw, []
    parser = build_parser()
    args = parser.parse_args(own)
    if args.probe_input and args.mode != "ready":
        parser.error("--probe-input is only supported with --mode ready")
    if not os.path.exists(args.binary) and shutil.which(args.binary) is None:
        parser.error(f"binary not found: {args.binary}")
    runs_requested = args.runs if args.runs is not None else (5 if args.mode == "ready" else 1)
    required = tuple(args.require) if args.require else DEFAULT_REQUIRED
    labels = dict(args.label)
    cfg = RunConfig(
        mode=args.mode,
        timeout=args.timeout,
        ready_timeout=args.ready_timeout,
        quit_timeout=args.quit_timeout,
        cancel_timeout=args.cancel_timeout,
        kill_grace=args.kill_grace,
        idle_window=args.idle_window,
        idle_interval=args.idle_interval,
        probe_input=args.probe_input,
        probe_char=args.probe_char,
        probe_timeout=args.probe_timeout,
        cancel_settle=args.cancel_settle,
        cancel_evidence=(
            tuple(args.cancel_evidence)
            if args.cancel_evidence
            else (() if args.cancel_evidence_regex else DEFAULT_CANCEL_EVIDENCE)
        ),
        cancel_evidence_regex=tuple(args.cancel_evidence_regex or ()),
        cols=args.cols,
        rows=args.rows,
        max_capture_bytes=args.max_capture_bytes,
        required=required,
        header_marker=args.header_marker,
        quit_keys=args.quit_hex,
        cancel_keys=args.cancel_hex,
    )
    command = [args.binary] + binary_args
    evidence_binary = (
        args.binary if os.path.exists(args.binary) else (shutil.which(args.binary) or args.binary)
    )
    methodology = collect_methodology(
        evidence_binary, command, args.build_profile, labels, required, args.cols, args.rows
    )
    if args.lockfile:
        provenance = collect_provenance(args.lockfile)
        for key in ("lockfile", "lockfile_sha256"):
            methodology[key] = provenance[key]
    runs = [run_measurement(cfg, command, index) for index in range(runs_requested)]
    record = {
        "schema": SCHEMA,
        "methodology": methodology,
        "config": {
            "mode": cfg.mode,
            "runs": runs_requested,
            "timeout_s": cfg.timeout,
            "ready_timeout_s": cfg.ready_timeout,
            "idle_window_s": cfg.idle_window,
            "probe_input": cfg.probe_input,
            "cancel_timeout_s": cfg.cancel_timeout,
            "cancel_settle_s": cfg.cancel_settle,
            "cancel_evidence": list(cfg.cancel_evidence),
            "cancel_evidence_regex": list(cfg.cancel_evidence_regex),
        },
        "runs": runs,
        "summary": summarize_runs(cfg, runs),
    }
    if args.json:
        with open(args.json, "w", encoding="utf-8") as handle:
            json.dump(record, handle, indent=2)
            handle.write("\n")
        report_stream = sys.stdout
    else:
        print(json.dumps(record, indent=2))
        report_stream = sys.stderr
    for run in runs:
        ready = run["ready"]
        ready_text = (
            f"status={ready['status']} ms={ready['ms']} missing={ready['missing']}"
            if ready is not None
            else "status=not-attempted"
        )
        print(
            f"[{cfg.mode}] run {run['run_index'] + 1}/{runs_requested} "
            f"{ready_text} outcome={run['outcome']} "
            f"capture={run['capture']['retained_bytes']}B "
            f"dropped={run['capture']['dropped_bytes']}B",
            file=report_stream,
        )
        if run["idle"] is not None:
            print(
                f"[idle] run {run['run_index'] + 1} status={run['idle']['status']} "
                f"window={run['idle']['window_s']:.3f}s "
                f"cpu={run['idle']['cpu_seconds']} "
                f"idle_cpu={run['idle']['idle_cpu_percent']} "
                f"wakeups={run['idle']['wakeups']['status']}",
                file=report_stream,
            )
        if run["cancel"] is not None:
            print(
                f"[cancel] run {run['run_index'] + 1} status={run['cancel']['status']} "
                f"marker_ms={run['cancel']['cancel_marker_observed_ms']} "
                f"to_exit_ms={run['cancel']['cancel_to_exit_ms']} "
                f"evidence_source={run['cancel'].get('evidence_source')}",
                file=report_stream,
            )
        if run["input_probe"] is not None:
            print(
                f"[input] run {run['run_index'] + 1} status={run['input_probe']['status']} "
                f"ms={run['input_probe']['ms']}",
                file=report_stream,
            )
    summary = record["summary"]
    ready_summary = summary["ready_ms"]
    print(
        f"summary: runs={summary['runs']} outcomes={summary['outcomes']} "
        f"ready n={ready_summary['n']} p50={ready_summary['p50_ms']} "
        f"p95={ready_summary['p95_ms']} max={ready_summary['max_ms']}",
        file=report_stream,
    )
    failures = sum(1 for run in runs if run["outcome"] in FAILURE_OUTCOMES)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
