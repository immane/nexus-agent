#!/usr/bin/env python3
"""Generic fresh-process startup measurement harness (stdlib only).

Measures wall-clock time from just before process spawn to process exit,
over N repeated fresh-process launches of an arbitrary binary. Reports
nearest-rank p50/p95/max and per-target max RSS over successful samples,
separately for warm-cache and cold-cache runs. Generic tool: makes no
claims about any specific project, and spawn-to-exit is a proxy, not the
"first usable interface" startup definition in docs/design/05-performance.md.

Usage:
    python3 startup.py /path/to/binary [-- arg1 arg2 ...]
    python3 startup.py /path/to/binary -n 50 --mode both -- build-arg

Sample honesty:
- Only launches that exit 0 feed the timing and RSS summaries. Timeouts,
  nonzero exits, spawn failures, and status-collection failures are kept
  as invalid samples with their exit code/signal/error evidence.
- A daemon watcher blocks in os.wait4 (POSIX) or Popen.wait with no
  timeout, so completion is noticed promptly; an independent event wait
  is the bounded watchdog. Timeouts kill and reap with a bounded wait
  rather than polling.
- Cold-cache honesty: a new process does NOT imply a cold filesystem
  cache. This script never purges the OS cache or runs privileged,
  install, or network commands on its own. Pass --purge-cmd for cold
  attempts; its exit status and timeout are enforced, and a purge failure
  invalidates the cold mode. A purge command exiting 0 does not prove the
  cache was reset: runs are labeled user-supplied cache prep (reset not
  verified), and no runtime check here can prove a reset. See README.md
  for example purge commands.
- A timeout kills the target's own process group (the target is launched
  in a new session, so killpg is scoped to the spawned group) and reaps
  the direct child. Processes that escape the group are not tracked.
"""

import argparse
import hashlib
import json
import math
import os
import platform
import signal
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone

SCHEMA = "nexus.perf.startup/1"
OK = "ok"
INVALID_OUTCOMES = ("nonzero", "timeout", "launch_error", "wait_error")

# Bounded wait for the watcher to reap after a timeout kill. SIGKILL cannot
# be ignored, but a process stuck in uninterruptible I/O still needs a limit.
REAP_AFTER_KILL_S = 5.0

POSIX_WAIT4 = hasattr(os, "wait4")


def percentile(sorted_ms, pct):
    """Nearest-rank percentile over an already-sorted list."""
    if not sorted_ms:
        return float("nan")
    if len(sorted_ms) == 1:
        return sorted_ms[0]
    rank = (pct / 100.0) * len(sorted_ms)
    idx = min(max(math.ceil(rank) - 1, 0), len(sorted_ms) - 1)
    return sorted_ms[idx]


def summarize(samples_ms):
    """Timing summary over successful sample durations (ms)."""
    ordered = sorted(samples_ms)
    if not ordered:
        return {"n": 0, "p50_ms": None, "p95_ms": None, "max_ms": None}
    return {
        "n": len(ordered),
        "p50_ms": percentile(ordered, 50),
        "p95_ms": percentile(ordered, 95),
        "max_ms": ordered[-1],
    }


def positive_seconds(value):
    """argparse type: reject zero, negative, NaN, and infinite timeouts."""
    try:
        seconds = float(value)
    except (TypeError, ValueError):
        raise argparse.ArgumentTypeError(f"invalid number: {value!r}")
    if not math.isfinite(seconds) or seconds <= 0:
        raise argparse.ArgumentTypeError("timeout must be a positive finite number of seconds")
    return seconds


class PurgeError(RuntimeError):
    """Cache-prep command failed; the cold mode is invalid, not measured."""

    def __init__(self, reason, detail, *, command=None, elapsed_ms=None,
                 exit_code=None, signal=None, samples=None, sample_index=None):
        super().__init__(detail)
        self.reason = reason
        self.detail = detail
        self.command = command
        self.elapsed_ms = elapsed_ms
        self.exit_code = exit_code
        self.signal = signal
        self.samples = list(samples or [])
        self.sample_index = sample_index


SHA256_LIMIT_BYTES = 256 * 1024 * 1024


def _sha256_file(path, limit_bytes=SHA256_LIMIT_BYTES):
    """SHA-256 of a local file; never raises. Large files are skipped."""
    try:
        if os.path.getsize(path) > limit_bytes:
            return f"skipped (file exceeds {limit_bytes // (1024 * 1024)} MiB)"
    except OSError:
        return "unknown"
    digest = hashlib.sha256()
    try:
        with open(path, "rb") as fh:
            for chunk in iter(lambda: fh.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError:
        return "unknown"
    return digest.hexdigest()


def collect_repo_provenance():
    """Best-effort commit/dirty/lockfile provenance; bounded, never fatal."""
    info = {"repo_commit": None, "repo_dirty": None, "lockfile": None, "lockfile_sha256": None}
    try:
        head = subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True,
                              text=True, timeout=5)
        if head.returncode == 0:
            info["repo_commit"] = head.stdout.strip()
            status = subprocess.run(["git", "status", "--porcelain"], capture_output=True,
                                    text=True, timeout=5)
            if status.returncode == 0:
                info["repo_dirty"] = bool(status.stdout.strip())
    except (OSError, subprocess.SubprocessError):
        pass
    lockfile = os.path.join(os.getcwd(), "Cargo.lock")
    if os.path.isfile(lockfile):
        info["lockfile"] = lockfile
        info["lockfile_sha256"] = _sha256_file(lockfile)
    return info


def collect_environment(binary, build_profile, purge_cmd):
    info = {
        "timestamp_utc": datetime.now(timezone.utc).isoformat(),
        "os": f"{platform.system()} {platform.release()} ({platform.version()})",
        "arch": platform.machine(),
        "processor": platform.processor() or "unknown",
        "python": platform.python_version(),
        "binary": binary,
        "build_profile": build_profile,
        "cold_purge_cmd": purge_cmd or "none (cache not purged)",
    }
    try:
        info["binary_bytes"] = os.path.getsize(binary)
    except OSError:
        info["binary_bytes"] = "unknown"
    info["binary_sha256"] = _sha256_file(binary)
    info.update(collect_repo_provenance())
    # Best-effort hardware model string; absent on many systems.
    hw = "unknown"
    try:
        if sys.platform == "darwin":
            hw = subprocess.run(
                ["sysctl", "-n", "hw.model"], capture_output=True, text=True, timeout=10
            ).stdout.strip() or "unknown"
        elif sys.platform.startswith("linux"):
            with open("/proc/cpuinfo", encoding="utf-8", errors="replace") as f:
                for line in f:
                    if line.startswith("model name"):
                        hw = line.split(":", 1)[1].strip()
                        break
    except Exception:
        pass
    info["hardware"] = hw
    return info


def rss_unit():
    if sys.platform == "darwin":
        return "bytes (macOS ru_maxrss)"
    if sys.platform.startswith("linux"):
        return "kilobytes (Linux ru_maxrss)"
    return "platform-specific ru_maxrss units"


def rss_method():
    if POSIX_WAIT4:
        return f"os.wait4 rusage of the directly waited target ({rss_unit()})"
    return "unavailable (os.wait4 not available on this platform)"


def _ms_since(start_ns):
    return (time.perf_counter_ns() - start_ns) / 1e6


def _decode_status(status):
    """Map a POSIX wait status to (exit_code, signal)."""
    if os.WIFEXITED(status):
        return os.WEXITSTATUS(status), None
    if os.WIFSIGNALED(status):
        return None, os.WTERMSIG(status)
    return None, None


def _kill(proc, group=False):
    """Best-effort SIGKILL for a not-yet-reaped child.

    Polls first so a reaped pid/pgid (possibly recycled) is never signalled.
    While the child is unreaped it still anchors its process group, so
    killpg(pid) is scoped to the group started for this spawn.
    """
    if proc.poll() is not None:
        return
    sig = getattr(signal, "SIGKILL", signal.SIGTERM)
    if group and hasattr(os, "killpg"):
        try:
            os.killpg(proc.pid, sig)
            return
        except OSError:
            pass
    try:
        proc.kill()
    except OSError:
        pass


def _wait_bounded(proc, timeout, kill_group=False):
    """Wait up to `timeout` seconds for `proc`, then kill and reap it.

    A daemon watcher blocks with no timeout (os.wait4, or Popen.wait when
    os.wait4 is unavailable), so exit notification is prompt instead of
    polled. The caller's event wait is the independent bounded watchdog;
    the watcher owns reaping, so no zombie or orphan reap is left behind.
    Returns a dict with timed_out, exit_code, signal, rss, and error.
    """
    done = threading.Event()
    outcome = {}

    def watcher():
        try:
            if POSIX_WAIT4:
                _, status, usage = os.wait4(proc.pid, 0)
                exit_code, signum = _decode_status(status)
                # Keep the Popen object consistent so __del__ never polls.
                proc.returncode = exit_code if exit_code is not None else (-signum if signum else 0)
                outcome["rss"] = getattr(usage, "ru_maxrss", None)
            else:
                proc.wait()
                returncode = proc.returncode
                if returncode >= 0:
                    exit_code, signum = returncode, None
                else:
                    exit_code, signum = None, -returncode
            outcome["exit_code"] = exit_code
            outcome["signal"] = signum
        except ChildProcessError:
            outcome["error"] = "child status was already collected"
        except OSError as exc:
            outcome["error"] = f"{type(exc).__name__}: {exc}"
        finally:
            done.set()

    thread = threading.Thread(target=watcher, name="startup-wait", daemon=True)
    thread.start()

    try:
        timed_out = not done.wait(timeout)
    except BaseException:
        _kill(proc, group=kill_group)
        raise

    if timed_out:
        _kill(proc, group=kill_group)
        if not done.wait(REAP_AFTER_KILL_S):
            outcome["error"] = f"not reaped within {REAP_AFTER_KILL_S:g}s after kill"

    # If Popen.poll() inside kill() collected the status, recover it.
    if outcome.get("error") and outcome.get("exit_code") is None and proc.returncode is not None:
        returncode = proc.returncode
        outcome["exit_code"], outcome["signal"] = (
            (returncode, None) if returncode >= 0 else (None, -returncode)
        )
        outcome["error"] = None

    return {
        "timed_out": timed_out,
        "exit_code": outcome.get("exit_code"),
        "signal": outcome.get("signal"),
        "rss": outcome.get("rss"),
        "error": outcome.get("error"),
    }


def measure_once(argv, timeout):
    """One fresh-process launch; returns a sample dict.

    Target failures (spawn error, timeout, nonzero exit) become invalid
    samples with retained evidence instead of exceptions.
    """
    start = time.perf_counter_ns()
    try:
        # Own session/process group so a timeout can kill grandchildren
        # scoped to this spawn; non-POSIX platforms ignore the flag and
        # fall back to a direct-child kill.
        proc = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                start_new_session=True)
    except OSError as exc:
        return {
            "outcome": "launch_error",
            "elapsed_ms": _ms_since(start),
            "max_rss": None,
            "exit_code": None,
            "signal": None,
            "error": f"{type(exc).__name__}: {exc}",
        }

    waited = _wait_bounded(proc, timeout, kill_group=True)
    sample = {
        "outcome": "wait_error",
        "elapsed_ms": _ms_since(start),
        "max_rss": waited["rss"],
        "exit_code": waited["exit_code"],
        "signal": waited["signal"],
        "error": waited["error"],
    }
    if waited["timed_out"]:
        sample["outcome"] = "timeout"
        detail = f"exceeded timeout {timeout:g}s; termination requested"
        if waited["error"]:
            detail += f"; {waited['error']}"
        sample["error"] = detail
    elif waited["error"]:
        sample["outcome"] = "wait_error"
    elif waited["signal"] is not None:
        sample["outcome"] = "nonzero"
        sample["error"] = f"terminated by signal {waited['signal']}"
    elif waited["exit_code"] == 0:
        sample["outcome"] = OK
        sample["error"] = None
    else:
        sample["outcome"] = "nonzero"
    return sample


def run_purge(purge_cmd, timeout):
    """Run the user-supplied cache-prep command once.

    Returns evidence on success (exit 0 within `timeout`). Raises
    PurgeError otherwise so the caller can invalidate the cold mode.
    """
    start = time.perf_counter_ns()
    try:
        proc = subprocess.Popen(purge_cmd, shell=True, start_new_session=True)
    except OSError as exc:
        raise PurgeError("launch_error", f"{type(exc).__name__}: {exc}",
                         command=purge_cmd)
    waited = _wait_bounded(proc, timeout, kill_group=True)
    evidence = {
        "command": purge_cmd,
        "elapsed_ms": _ms_since(start),
        "exit_code": waited["exit_code"],
        "signal": waited["signal"],
    }
    if waited["timed_out"]:
        raise PurgeError(
            "timeout",
            f"purge command did not exit within {timeout:g}s; termination requested",
            **evidence,
        )
    if waited["error"]:
        raise PurgeError("reap_error", f"purge command status failed: {waited['error']}",
                         **evidence)
    if waited["exit_code"] != 0 or waited["signal"] is not None:
        raise PurgeError(
            "nonzero",
            f"purge command failed (exit_code={waited['exit_code']}, signal={waited['signal']})",
            **evidence,
        )
    return evidence


def run_batch(argv, n, timeout, purge_cmd=None):
    """Run n fresh launches; purge before each sample when purge_cmd is set.

    The purge command must exit 0 within `timeout`; otherwise PurgeError is
    raised with the samples collected so far attached as evidence. Target
    failures never raise: they are recorded as invalid samples.
    """
    samples = []
    for index in range(n):
        if purge_cmd:
            try:
                run_purge(purge_cmd, timeout)
            except PurgeError as exc:
                exc.samples = samples
                exc.sample_index = index
                raise
        sample = measure_once(argv, timeout)
        sample["index"] = index
        samples.append(sample)
    return samples


def summarize_samples(samples):
    """Summarize raw sample dicts: successful stats plus invalid counts."""
    successful = [s for s in samples if s["outcome"] == OK]
    stats = summarize([s["elapsed_ms"] for s in successful])
    invalid = {}
    for name in INVALID_OUTCOMES:
        count = sum(1 for s in samples if s["outcome"] == name)
        if count:
            invalid[name] = count
    rss_values = [s["max_rss"] for s in successful if s["max_rss"] is not None]
    stats.update({
        "attempted": len(samples),
        "successful": len(successful),
        "invalid": invalid,
        "max_rss": max(rss_values) if rss_values else None,
    })
    return stats


def _fmt_ms(value):
    return "n/a" if value is None else f"{value:.3f}ms"


def _print_mode(entry):
    mode = entry["mode"]
    results = entry["results"]
    invalid = ", ".join(f"{name}={count}" for name, count in results["invalid"].items()) or "none"
    print(f"[{mode}] successful={results['successful']}/{results['attempted']} "
          f"p50={_fmt_ms(results['p50_ms'])} p95={_fmt_ms(results['p95_ms'])} "
          f"max={_fmt_ms(results['max_ms'])} invalid=[{invalid}]")
    if results["max_rss"] is not None:
        print(f"[{mode}] max_rss_successful={results['max_rss']:.0f} ({rss_unit()})")
    for sample in entry["samples"]:
        if sample["outcome"] != OK:
            print(f"[{mode}] invalid sample #{sample['index']}: outcome={sample['outcome']} "
                  f"exit_code={sample['exit_code']} signal={sample['signal']} "
                  f"elapsed={_fmt_ms(sample['elapsed_ms'])} error={sample['error']}")


def build_parser():
    ap = argparse.ArgumentParser(
        description="Measure fresh-process startup of any binary (stdlib only)."
    )
    ap.add_argument("binary", help="Path to the binary to launch repeatedly.")
    ap.add_argument("-n", "--runs", type=int, default=20,
                    help="Launches per cache mode (default: 20).")
    ap.add_argument("--mode", choices=["warm", "cold", "both"], default="warm",
                    help="warm: back-to-back launches; cold: run --purge-cmd before each launch; "
                         "both: warm then cold.")
    ap.add_argument("--purge-cmd", default=None,
                    help="Shell command run before each cold sample to drop filesystem caches. "
                         "Must exit 0 within --timeout or the cold mode is invalid.")
    ap.add_argument("--timeout", type=positive_seconds, default=60.0,
                    help="Per-launch and per-purge timeout in seconds; must be positive and "
                         "finite (default: 60).")
    ap.add_argument("--build-profile", default="unknown/external binary",
                    help="Label recorded in methodology (script cannot detect it).")
    ap.add_argument("--json", metavar="PATH", default=None,
                    help="Write the machine-readable record (methodology, raw samples, results) "
                         "to PATH.")
    return ap


def main(argv=None):
    raw = list(sys.argv[1:] if argv is None else argv)
    parser = build_parser()
    if "--" in raw:
        cut = raw.index("--")
        own, binary_args = raw[:cut], raw[cut + 1:]
    else:
        own, binary_args = raw, []
    args = parser.parse_args(own)
    if args.runs < 1:
        parser.error("--runs must be >= 1")

    launch_argv = [args.binary] + binary_args
    env = collect_environment(args.binary, args.build_profile, args.purge_cmd)
    env.update({
        "mode": args.mode,
        "runs_per_mode": args.runs,
        "launch_argv": launch_argv,
        "timeout_s": args.timeout,
        "purge_timeout_s": args.timeout,
        "timer": "time.perf_counter_ns() from before spawn to exit (wall clock)",
        "wait_notification": ("blocking os.wait4 with event watchdog" if POSIX_WAIT4
                              else "blocking Popen.wait with event watchdog"),
        "rss_method": rss_method(),
        "cache_reset_verified": False,
        "cold_cache_prep": (
            "user-supplied purge command configured for cold mode; exit 0 required per sample; "
            "cache reset not verified"
            if args.purge_cmd else "none (cache not purged; cold runs are unprepared)"
        ),
    })

    print("methodology:")
    for key, value in env.items():
        print(f"  {key}: {value}")

    modes = ["warm", "cold"] if args.mode == "both" else [args.mode]
    record = {"schema": SCHEMA, "methodology": env, "modes": []}
    exit_code = 0
    for mode in modes:
        purge = args.purge_cmd if mode == "cold" else None
        if mode == "cold" and not purge:
            print("note: cold-cache requested but no --purge-cmd given; "
                  "OS cache was NOT reset (unprepared-cache runs).")
        entry = {
            "mode": mode,
            "valid": True,
            "invalid_reason": None,
            "cache_prep": ("purge configured; result pending (cache reset not verified)"
                           if purge else "none"),
            "purge_cmd": purge,
            "purge_runs": 0,
            "samples": [],
            "results": None,
        }
        try:
            samples = run_batch(launch_argv, args.runs, args.timeout, purge)
        except PurgeError as exc:
            entry["valid"] = False
            entry["invalid_reason"] = f"purge {exc.reason}: {exc.detail}"
            entry["cache_prep"] = "purge failed; cache reset not verified"
            entry["samples"] = exc.samples
            entry["purge_runs"] = len(exc.samples) + 1
            entry["purge_error"] = {
                "reason": exc.reason,
                "detail": exc.detail,
                "exit_code": exc.exit_code,
                "signal": exc.signal,
                "elapsed_ms": exc.elapsed_ms,
                "failed_at_sample": exc.sample_index,
            }
            print(f"[{mode}] invalid: purge {exc.reason}: {exc.detail} "
                  f"(at sample {exc.sample_index}, {len(exc.samples)} collected)")
            exit_code = 2
        else:
            entry["samples"] = samples
            entry["purge_runs"] = len(samples) if purge else 0
            if purge:
                entry["cache_prep"] = (
                    "user-supplied purge command exited 0 for every sample "
                    "(cache reset not verified)"
                )
            entry["results"] = summarize_samples(samples)
            _print_mode(entry)
            if purge:
                print(f"[{mode}] note: --purge-cmd exited 0 for every sample; the harness cannot "
                      "verify the OS cache was actually reset (user-supplied cache prep, "
                      "unverified). Do not present these as genuinely cold unless the reset "
                      "mechanism was verified externally.")
            # Any invalid sample means the validation batch is not clean:
            # successful samples keep their stats, failures keep evidence.
            if entry["results"]["invalid"]:
                exit_code = max(exit_code, 1)
        record["modes"].append(entry)

    if args.json:
        try:
            with open(args.json, "w", encoding="utf-8") as fh:
                json.dump(record, fh, indent=2)
                fh.write("\n")
        except OSError as exc:
            print(f"error: could not write --json {args.json}: {exc}", file=sys.stderr)
            return 2
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
