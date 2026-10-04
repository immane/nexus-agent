#!/usr/bin/env python3
"""Generic fresh-process startup measurement harness (stdlib only).

Measures wall-clock time from just before process spawn to process exit,
over N repeated fresh-process launches of an arbitrary binary. Reports
p50/p95/max and sample count, separately for warm-cache and cold-cache
runs. Generic tool: makes no claims about any specific project.

Usage:
    python3 startup.py /path/to/binary [-- arg1 arg2 ...]
    python3 startup.py /path/to/binary -n 50 --mode both -- build-arg

Cold-cache honesty: a new process does NOT imply a cold filesystem cache.
This script never purges the OS cache itself (that needs privileges).
For genuinely cold runs, pass --purge-cmd (run before each cold sample,
e.g. a command that drops caches); otherwise cold samples are labeled
with cache_prep "none" and must be read as unprepared-cache runs.
See README.md for the macOS/Linux purge commands.
"""

import argparse
import os
import platform
import subprocess
import sys
import time
from datetime import datetime, timezone


def percentile(sorted_ms, pct):
    """Nearest-rank percentile over an already-sorted list."""
    if not sorted_ms:
        return float("nan")
    if len(sorted_ms) == 1:
        return sorted_ms[0]
    rank = (pct / 100.0) * len(sorted_ms)
    import math

    idx = min(max(math.ceil(rank) - 1, 0), len(sorted_ms) - 1)
    return sorted_ms[idx]


def summarize(samples_ms):
    s = sorted(samples_ms)
    return {
        "n": len(s),
        "p50_ms": percentile(s, 50),
        "p95_ms": percentile(s, 95),
        "max_ms": max(s) if s else float("nan"),
    }


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


def measure_once(argv, timeout):
    """One fresh-process launch; returns (elapsed_ms, max_rss_delta or None)."""
    try:
        import resource

        have_resource = hasattr(resource, "getrusage")
    except ImportError:
        resource = None
        have_resource = False
    before = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss if have_resource else None
    start = time.perf_counter_ns()
    proc = subprocess.run(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=timeout)
    elapsed_ms = (time.perf_counter_ns() - start) / 1e6
    rss = None
    if have_resource:
        # ru_maxrss is a cumulative high-water mark, so per-launch deltas are
        # meaningless; return the absolute mark and let the caller take max.
        rss = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    return elapsed_ms, rss, proc.returncode


def run_batch(argv, n, timeout, purge_cmd=None):
    samples, rss_abs, failures = [], [], 0
    for _ in range(n):
        if purge_cmd:
            subprocess.run(purge_cmd, shell=True)
        ms, rss, rc = measure_once(argv, timeout)
        if rc != 0:
            failures += 1
        samples.append(ms)
        if rss is not None:
            rss_abs.append(rss)
    return samples, rss_abs, failures


def rss_unit():
    if sys.platform == "darwin":
        return "bytes (macOS ru_maxrss)"
    if sys.platform.startswith("linux"):
        return "kilobytes (Linux ru_maxrss)"
    return "platform-specific ru_maxrss units"


def main():
    ap = argparse.ArgumentParser(description="Measure fresh-process startup of any binary (stdlib only).")
    ap.add_argument("binary", help="Path to the binary to launch repeatedly.")
    ap.add_argument("-n", "--runs", type=int, default=20, help="Launches per cache mode (default: 20).")
    ap.add_argument("--mode", choices=["warm", "cold", "both"], default="warm",
                    help="warm: back-to-back launches; cold: run --purge-cmd before each launch; both: warm then cold.")
    ap.add_argument("--purge-cmd", default=None,
                    help="Shell command run before each cold sample to drop filesystem caches (needs privileges).")
    ap.add_argument("--timeout", type=float, default=60.0, help="Per-launch timeout in seconds.")
    ap.add_argument("--build-profile", default="unknown/external binary",
                    help="Label recorded in methodology (script cannot detect it).")
    raw = sys.argv[1:]
    if "--" in raw:
        cut = raw.index("--")
        own, binary_args = raw[:cut], raw[cut + 1:]
    else:
        own, binary_args = raw, []
    args = ap.parse_args(own)

    argv = [args.binary] + binary_args
    if args.runs < 1:
        ap.error("--runs must be >= 1")

    env = collect_environment(args.binary, args.build_profile, args.purge_cmd)
    print("methodology:")
    for k, v in env.items():
        print(f"  {k}: {v}")
    print(f"  runs_per_mode: {args.runs}")
    print(f"  mode: {args.mode}")
    print("  timer: time.perf_counter_ns() around subprocess.run (spawn to exit, wall-clock)")
    print(f"  child_max_rss_units: {rss_unit()}")

    modes = ["warm", "cold"] if args.mode == "both" else [args.mode]
    for mode in modes:
        purge = args.purge_cmd if mode == "cold" else None
        if mode == "cold" and not purge:
            print("note: cold-cache requested but no --purge-cmd given; "
                  "OS cache was NOT reset (unprepared-cache runs).")
        samples, rss_samples, failures = run_batch(argv, args.runs, args.timeout, purge)
        stats = summarize(samples)
        print(f"[{mode}] n={stats['n']} p50={stats['p50_ms']:.3f}ms "
              f"p95={stats['p95_ms']:.3f}ms max={stats['max_ms']:.3f}ms "
              f"nonzero_exit={failures}")
        if rss_samples:
            print(f"[{mode}] child_max_rss_highwater={max(rss_samples):.0f} ({rss_unit()}; "
                  "cumulative max, not per-launch)")


if __name__ == "__main__":
    main()
