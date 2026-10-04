# Startup measurement harness (skeleton)

Generic, dependency-free (Python 3 stdlib only) harness that measures
fresh-process startup for **any** binary path passed as an argument.
It makes no claims about this project; there is currently no built
binary in this repo to benchmark.

## Usage

```sh
python3 tools/perf/startup.py /path/to/binary -n 20
python3 tools/perf/startup.py /path/to/binary -n 50 --mode both -- --headless run
python3 tools/perf/startup.py /path/to/binary --mode cold --purge-cmd 'sync && echo 3 | sudo tee /proc/sys/vm/drop_caches'
```

Options: `-n/--runs` (launches per mode), `--mode warm|cold|both`
(default `warm`), `--purge-cmd`, `--timeout`, `--build-profile`
(a label recorded in the methodology header; the script cannot detect it).

Output: methodology header (OS, arch, hardware, binary size, timer,
build-profile label) plus one line per mode with
`n`, `p50`, `p95`, `max` (ms) and nonzero-exit count.

## Warm vs cold

- **Warm-cache**: back-to-back launches; the OS filesystem cache is hot.
  This is the default and needs no privileges.
- **Cold-cache**: genuinely cold page cache. A new process does **not**
  imply a cold cache, so the script never purges caches itself. Pass
  `--purge-cmd` to run a privileged cache drop before each cold sample.
  Without it, cold runs are labeled unprepared and must not be reported
  as true cold-cache numbers.

## macOS specifics

- The script's child-RSS figures come from `resource.getrusage`
  (`ru_maxrss`), which is **bytes on macOS** and kilobytes on Linux;
  units are printed with the results.
- Alternative manual method: `/usr/bin/time -l ./binary` prints, among
  others, `maximum resident set size` (bytes), `average ... mem`,
  page faults (`minor/major ... faults`), voluntary/involuntary context
  switches, and `elapsed` wall time. Useful fields: `real`/`user`/`sys`
  and `maximum resident set size`.
- True cold cache on macOS requires `sudo purge` before samples (closes
  no apps but needs admin rights). Document whether it was used.

## Linux method (documented, not executed)

This host is macOS; the Linux procedure below is recorded for later and
was **not run**:

```sh
# 1. Build the binary, note profile/features.
# 2. Warm: python3 tools/perf/startup.py ./binary -n 100 --mode warm
# 3. Cold: python3 tools/perf/startup.py ./binary -n 20 --mode cold \
#      --purge-cmd 'sync && echo 3 | sudo tee /proc/sys/vm/drop_caches'
# 4. Compare like-for-like: same machine, OS, build profile, workload.
```

Linux notes: `ru_maxrss` is kilobytes; `/usr/bin/time -v` gives
"Maximum resident set size (kbytes)" plus page-fault counts.
Record distro, kernel, CPU, and build profile with every result.

## Limitations

- Measures spawn-to-exit wall time only (generic proxy), not the
  "first usable interface" startup definition in `docs/design/05-performance.md`.
- No PTY/terminal setup, no TUI readiness detection, no idle/peak memory,
  CPU, wakeups, streaming, or dispatch-overhead metrics.
- Cold numbers without a documented purge are unprepared-cache runs.
- No Linux execution has been performed from this host.
