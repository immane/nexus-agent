# Startup measurement harness

Generic, dependency-free (Python 3 stdlib only) harness that measures
fresh-process startup for **any** binary path passed as an argument. It
makes no claims about this project; there may be no built binary in this
repo to benchmark.

What it measures: wall-clock time from just before process spawn to
process exit, over repeated fresh-process launches (`-n`), reported as
nearest-rank p50/p95/max over **successful** samples, separately per
cache mode. This is a spawn-to-exit proxy; it is **not** the "first usable
interface" startup definition in `docs/design/05-performance.md`. There is
no PTY setup, no readiness signal, and no TUI/headless interactivity check;
nothing here should be reported as first-interactive startup.

## Usage

```sh
python3 tools/perf/startup.py /path/to/binary -n 20
python3 tools/perf/startup.py /path/to/binary -n 50 --mode both -- --headless run
python3 tools/perf/startup.py /path/to/binary --mode cold --purge-cmd 'sync && echo 3 | sudo tee /proc/sys/vm/drop_caches'
python3 tools/perf/startup.py /path/to/binary -n 20 --json results.json -- --headless run
```

Options: `binary [-- args...]`, `-n/--runs` (launches per mode),
`--mode warm|cold|both` (default `warm`), `--purge-cmd`, `--timeout`,
`--build-profile` (a label recorded in the methodology header; the script
cannot detect it). These original flags keep their names and meanings.
`--json PATH` is additive and writes the machine-readable record.

`--timeout` must be positive and finite; zero, negative, NaN, and infinite
values are rejected. It bounds both each target launch and each purge run.
Exceeding it kills the target's own process group (the target is launched in
a new session, so the group kill is scoped to the spawned process) and
records a `timeout` sample instead of crashing the batch.

## Sample outcomes

Each launch is one raw sample with exactly one outcome:

- `ok`: exited 0. Only `ok` samples feed p50/p95/max and max RSS.
- `nonzero`: exited with a nonzero code; the code is retained.
- `timeout`: watchdog expired; termination was requested and observed
  exit/signal evidence is retained. A process that exits in the watchdog race
  may have no signal, so the record does not claim it was killed.
- `launch_error`: the harness could not spawn it (e.g. missing binary);
  the `OSError` text is retained.
- `wait_error`: spawned but its status could not be collected.

Invalid samples are printed individually with their exit code, signal,
elapsed time, and error, and are kept in the JSON record. They are never
counted as successful samples or mixed into the timing percentiles.

A validation batch is clean only if **every** sample is `ok`: any
`nonzero`, `timeout`, `launch_error`, or `wait_error` sample makes the
harness exit nonzero even when other samples succeeded. Successful samples
keep their stats; failed samples keep their raw evidence.

Wait notification uses a daemon watcher that blocks with **no timeout**
(`os.wait4` on POSIX, `Popen.wait()` elsewhere), so completion is noticed
promptly rather than by polling. The caller waits on an event as an
independent bounded watchdog; on timeout the harness requests termination of
the target's process group and reaps the direct child with a bounded post-
termination wait (`REAP_AFTER_KILL_S`), so a hanging target cannot stall the harness or
leave an unreaped zombie behind.

## Per-target RSS

On POSIX (`os.wait4` available), each sample's max RSS comes from the wait
status of that exact child. Helper processes (`sysctl`, purge shells) and
earlier launches cannot contaminate it, unlike a cumulative
`RUSAGE_CHILDREN.ru_maxrss` high-water mark. Units follow the platform:
bytes on macOS, kilobytes on Linux; the unit is printed and recorded.
Max RSS is summarized over successful samples only. On platforms without
`os.wait4`, timing still works but RSS is reported as unavailable.

Linux child high-water RSS can include the pre-exec fork footprint of the
launching process. Exact-child attribution does not guarantee post-exec-only
memory accounting. Run the CLI from a small, fresh harness process rather than
embedding measurements in a large long-lived Python process. RSS magnitude tests
use a freshly exec'd harness for this reason; they retain their allocation and
helper-contamination checks and do not subtract or clamp the kernel evidence.

## Warm vs cold

- **Warm-cache**: back-to-back launches; the OS filesystem cache is hot.
  This is the default and needs no privileges.
- **Cold-cache attempts**: `--mode cold` only means "run `--purge-cmd`
  before each sample". A new process does **not** imply a cold cache, and
  the harness never purges caches itself. The command must exit 0 within
  `--timeout`; a nonzero exit, timeout, or launch failure makes the whole
  cold mode invalid. Partial samples are retained as evidence in `--json`
   output, no cold summary is produced, and the process exits 2. `purge_runs`
   counts attempted purge commands, including the failed attempt.

**Purge success does not prove a cache reset.** Exit 0 only shows that the
user-supplied command ran and returned success. No portable runtime check
can prove that the OS page cache was actually dropped, and this harness
makes no such claim: successful cold runs are labeled as user-supplied purge
commands exiting 0 for every sample (cache reset not verified), while a
failed purge is labeled explicitly as failed; the methodology records
`cache_reset_verified: false`. Report successful runs as user-supplied cache
prep, not genuinely cold, unless the reset mechanism was verified
externally (for example by an independent probe or platform-specific
evidence). Without `--purge-cmd`, cold runs are labeled `cache_prep: none`
and are unprepared-cache runs.

The harness itself never runs privileged commands, installs anything, or
uses the network. `--purge-cmd` is executed exactly as supplied, in its own
process session so a timed-out purge receives a process-group termination
request.

Exit status: `0` every sample in every requested mode was `ok`; `1` at
least one invalid sample (`nonzero`/`timeout`/`launch_error`/`wait_error`)
even if other samples succeeded; `2` a cold mode was invalidated by a
failed purge (or CLI/JSON output error).

## JSON record (`--json PATH`)

Schema `nexus.perf.startup/1`, written with no `NaN` values:

- `methodology`: timestamp, OS/release/version, architecture, processor,
  hardware model, Python version, binary path, size, and SHA-256 (files up
  to 256 MiB; larger files are marked skipped), build-profile label, git
  `repo_commit`/`repo_dirty` and `Cargo.lock` SHA-256 when present in the
  working directory, timer and wait-notification method, RSS method and
  units, runs per mode, mode, timeouts, purge command, and
  `cache_reset_verified: false`.
- `modes[]`: one entry per requested mode with `valid`,
  `invalid_reason`, `cache_prep`, `purge_cmd`, `purge_runs`, the raw
  `samples` (index, outcome, `elapsed_ms`, `max_rss`, `exit_code`,
  `signal`, `error`), and `results` (`successful`, `attempted`,
  p50/p95/max over successful samples, invalid counts, max RSS over
  successful samples). A mode invalidated by a purge failure also carries
  `purge_error` evidence and any partial samples.

## macOS specifics

- `/usr/bin/time -l ./binary` prints, among others, `maximum resident set
  size` (bytes), page faults, context switches, and `real`/`user`/`sys`.
- True cold cache on macOS requires `sudo purge` before samples; the
  harness only runs it if it is passed as `--purge-cmd`, never by default.

## Linux method (documented, not executed)

This host is macOS; the Linux procedure below is recorded for later and
was **not run** here:

```sh
# 1. Build the binary, note profile/features.
# 2. Warm: python3 tools/perf/startup.py ./binary -n 100 --mode warm
# 3. Cold: python3 tools/perf/startup.py ./binary -n 20 --mode cold \
#      --purge-cmd 'sync && echo 3 | sudo tee /proc/sys/vm/drop_caches'
# 4. Compare like-for-like: same machine, OS, build profile, workload.
```

Linux notes: `ru_maxrss` is kilobytes; `/usr/bin/time -v` gives
"Maximum resident set size (kbytes)" plus page-fault counts. Record distro,
kernel, CPU, and build profile with every result. The purge command above
is user-supplied; the harness enforces its exit status and timeout but
does not require or embed `sudo`.

## Tests

```sh
python3 -m unittest discover -s tools/perf -p 'test_*.py'
```

The stdlib `unittest` suite covers percentile math, per-target RSS
isolation from prior children and purge helpers, purge nonzero/timeout
invalidation with partial-sample evidence, launch-error/nonzero/timeout
sample classification, timeout validation, mixed success/fail batches
(nonzero exit even when successes remain, with successful stats and raw
failed evidence kept), process-group cleanup of a timed-out target's
grandchild, watchdog `ResourceWarning` leaks, SHA-256 provenance, and the
`--json` CLI record (including purge-failure exit code 2 and all-invalid
exit code 1). POSIX-only cases skip on other platforms. Tests use only
benign local processes (Python itself, shell builtins, short sleeps).

## Limitations

- Measures spawn-to-exit wall time only (generic proxy); no PTY/terminal
  setup, no TUI readiness detection, no idle/peak memory, CPU, wakeups,
  streaming, or dispatch-overhead metrics. Do not present these numbers
  as first-interactive startup.
- Timeout, launch-error, and nonzero-exit launches are retained as invalid
  evidence; they are not performance samples.
- On a timeout the harness SIGKILLs the target's process group (the target
  is launched in a new session, and the kill is scoped to that spawned
  group while the direct child is unreaped). The direct child is reaped
  with `os.wait4`; processes that escape the group (for example by creating
  their own session) are not tracked, and total cleanup is not claimed.
  Purge commands run in their own session and are killed the same way.
- Per-target RSS requires POSIX `os.wait4`.
- Cold numbers without a documented purge are unprepared-cache runs.
- No Linux execution has been performed from this host.
