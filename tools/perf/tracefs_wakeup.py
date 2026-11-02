#!/usr/bin/env python3
"""Linux isolated-instance sched_wakeup experiment, Python 3 stdlib only.

Run as root on a dedicated test VM, with a dedicated unprivileged account:
  python3 tools/perf/tracefs_wakeup.py --user nexus-test --binary /absolute/path/nexus-tui --runs 3

The executable and account must be trusted test fixtures: changing HOME and cwd
is not a filesystem or network sandbox. No target arguments are accepted.
Only this program's unique tracefs instance is modified. SIGKILL or machine
failure can prevent cleanup; inspect the reported instance name in that case.
Events are scheduler wakeups of the initial TIDs, not machine wakeups, wakeups
caused by the target, or a census of threads that lived between snapshots.
"""

import argparse
import ctypes
import datetime
import fcntl
import hashlib
import json
import os
import platform
import pty
import pwd
import secrets
import select
import shutil
import signal
import subprocess
import struct
import sys
import tempfile
import termios
import time
from pathlib import Path

ROOT = Path('/sys/kernel/tracing')
MARKERS = (b'composer (fixed)',)
MAX_TRACE = 16 * 1024 * 1024
COUNTERS = ('overrun', 'commit overrun', 'dropped events')


def verify_tracefs():
    lines = Path('/proc/self/mountinfo').read_text().splitlines()
    if not any(len(parts := line.split()) > 6 and parts[4] == str(ROOT)
               and '-' in parts and parts[parts.index('-') + 1] == 'tracefs' for line in lines):
        raise RuntimeError(f'{ROOT} is not a tracefs mount')


def write(path, value):
    path.write_text(value, encoding='ascii')


def threads(pid):
    return sorted(int(p.name) for p in Path(f'/proc/{pid}/task').iterdir())


def stats(instance):
    result = {}
    for cpu in sorted((instance / 'per_cpu').glob('cpu[0-9]*')):
        values = {}
        for line in (cpu / 'stats').read_text().splitlines():
            key, sep, value = line.partition(':')
            if sep and key in COUNTERS:
                values[key] = int(value.strip())
        if set(values) != set(COUNTERS):
            raise RuntimeError(f'incomplete loss counters for {cpu.name}')
        result[cpu.name] = values
    if not result:
        raise RuntimeError('no per-CPU statistics')
    return result


def check_loss(before, after):
    if before.keys() != after.keys():
        raise RuntimeError('per-CPU statistics set changed')
    for cpu in before:
        for counter in COUNTERS:
            if before[cpu][counter] != after[cpu][counter]:
                raise RuntimeError(f'{cpu} {counter} changed: {before[cpu][counter]} -> {after[cpu][counter]}')


def verify_identity(pid, account):
    fields = {}
    for line in Path(f'/proc/{pid}/status').read_text().splitlines():
        key, sep, value = line.partition(':')
        if sep and key in ('Uid', 'Gid', 'Groups', 'CapEff', 'NoNewPrivs'):
            fields[key] = [int(x, 16 if key == 'CapEff' else 10) for x in value.split()]
    if (fields.get('Uid') != [account.pw_uid] * 4 or fields.get('Gid') != [account.pw_gid] * 4
            or fields.get('Groups') != [] or fields.get('CapEff') != [0] or fields.get('NoNewPrivs') != [1]):
        raise RuntimeError('target identity or supplementary groups do not match dedicated user')


class AnsiFilter:
    def __init__(self):
        self.state = 'text'

    def feed(self, data):
        output = bytearray()
        for byte in data:
            if self.state == 'text':
                if byte == 27:
                    self.state = 'esc'
                elif byte in (9, 10) or byte >= 32 and byte != 127:
                    output.append(byte)
            elif self.state == 'esc':
                self.state = {91: 'csi', 93: 'osc', 40: 'skip', 41: 'skip', 42: 'skip', 43: 'skip'}.get(byte, 'text')
            elif self.state == 'csi' and 64 <= byte <= 126:
                self.state = 'text'
            elif self.state == 'skip':
                self.state = 'text'
            elif self.state == 'osc':
                if byte == 7:
                    self.state = 'text'
                elif byte == 27:
                    self.state = 'osc-esc'
            elif self.state == 'osc-esc':
                self.state = 'text' if byte == 92 else 'osc'
        return bytes(output)


def await_frame(proc, master, timeout):
    deadline = time.monotonic() + timeout
    seen = set()
    tail = b''
    ansi = AnsiFilter()
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise RuntimeError('target exited before observed frame')
        if select.select([master], [], [], min(0.1, max(0, deadline - time.monotonic())))[0]:
            chunk = os.read(master, 65536)
            if not chunk:
                raise RuntimeError('PTY closed before observed frame')
            # SGR and cursor sequences are not text; markers may straddle reads.
            tail = (tail + ansi.feed(chunk))[-65536:]
            for marker in MARKERS:
                if marker in tail:
                    seen.add(marker)
            if len(seen) == len(MARKERS):
                return
    raise RuntimeError('timed out waiting for frame markers')


def stop_target(proc):
    if proc is None:
        return
    # poll() may have reaped an already-exited leader. Never signal a process
    # group after its identifier can have been reused by an unrelated process.
    if proc.poll() is not None:
        return
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    proc.wait(timeout=5)


def sample(instance, binary, account, seconds, timeout):
    result = {'status': 'failed', 'cleanup': {}}
    proc = None
    master = slave = None
    home = None
    try:
        home = tempfile.mkdtemp(prefix='nexus-wakeup-')
        os.chown(home, account.pw_uid, account.pw_gid)
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 80, 0, 0))
        def demote():
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
            os.setgroups([])
            os.setgid(account.pw_gid)
            os.setuid(account.pw_uid)
            if ctypes.CDLL(None, use_errno=True).prctl(38, 1, 0, 0, 0) != 0:  # PR_SET_NO_NEW_PRIVS
                raise OSError(ctypes.get_errno(), 'PR_SET_NO_NEW_PRIVS failed')
        env = {'HOME': home, 'XDG_CONFIG_HOME': home + '/config', 'XDG_DATA_HOME': home + '/data',
               'XDG_CACHE_HOME': home + '/cache', 'TERM': 'xterm-256color', 'PATH': '/usr/bin:/bin',
               'USER': account.pw_name, 'LOGNAME': account.pw_name}
        proc = subprocess.Popen([binary], stdin=slave, stdout=slave, stderr=slave,
                                cwd=home, env=env, preexec_fn=demote, close_fds=True)
        os.close(slave)
        slave = None
        result['pid'] = proc.pid
        verify_identity(proc.pid, account)
        await_frame(proc, master, timeout)
        verify_identity(proc.pid, account)
        initial = threads(proc.pid)
        result['threads_before'] = initial
        event = instance / 'events/sched/sched_wakeup'
        write(instance / 'tracing_on', '0')
        write(event / 'enable', '0')
        write(event / 'filter', ' || '.join(f'pid == {tid}' for tid in initial))
        write(instance / 'trace', '')
        write(event / 'enable', '1')
        before = stats(instance)
        result['stats_before'] = before
        result['window_start_utc'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        start = time.monotonic()
        write(instance / 'tracing_on', '1')
        deadline = start + seconds
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                raise RuntimeError('target exited during sampling')
            ready, _, _ = select.select([master], [], [], min(0.05, max(0, deadline - time.monotonic())))
            if ready:
                os.read(master, 65536)
        write(instance / 'tracing_on', '0')
        end = time.monotonic()
        result['window_end_utc'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        result['window_seconds'] = end - start
        after = stats(instance)
        result['stats_after'] = after
        final = threads(proc.pid)
        result['threads_after'] = final
        result['threads_added'] = sorted(set(final) - set(initial))
        result['threads_removed'] = sorted(set(initial) - set(final))
        check_loss(before, after)
        if initial != final:
            raise RuntimeError('thread set changed during observation')
        with (instance / 'trace').open('rb') as trace:
            data = trace.read(MAX_TRACE + 1)
        if len(data) > MAX_TRACE:
            raise RuntimeError('trace exceeds read limit')
        lines = [line for line in data.splitlines() if not line.startswith(b'#') and line.strip()]
        if any(b'sched_wakeup:' not in line for line in lines):
            raise RuntimeError('unexpected trace record')
        result['sched_wakeup_events'] = len(lines)
        result['events_per_second'] = len(lines) / (end - start)
        result['status'] = 'ok'
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as exc:
        result['error'] = str(exc)
    finally:
        try:
            write(instance / 'tracing_on', '0')
            result['cleanup']['tracing_disabled'] = True
        except OSError as exc:
            result['cleanup']['tracing_disabled'] = False
            result['cleanup']['trace_error'] = str(exc)
            result['status'] = 'failed'
        try:
            stop_target(proc)
            result['cleanup']['target_reaped'] = True
        except (OSError, subprocess.SubprocessError) as exc:
            result['cleanup']['target_reaped'] = False
            result['cleanup']['target_error'] = str(exc)
            result['status'] = 'failed'
        for fd in (master, slave):
            if fd is not None:
                os.close(fd)
        if home:
            try:
                shutil.rmtree(home)
                result['cleanup']['temporary_home_removed'] = True
            except OSError as exc:
                result['cleanup']['temporary_home_removed'] = False
                result['cleanup']['home_error'] = str(exc)
                result['status'] = 'failed'
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--user', required=True)
    parser.add_argument('--binary', required=True)
    parser.add_argument('--runs', type=int, default=3)
    args = parser.parse_args(argv)
    if sys.platform != 'linux' or os.geteuid() != 0:
        parser.error('requires Linux and root')
    try:
        account = pwd.getpwnam(args.user)
    except KeyError:
        parser.error('unknown user')
    if not account.pw_uid or not account.pw_gid or not 1 <= args.runs <= 100:
        parser.error('requires a non-root user and group and 1..100 runs')
    binary = os.path.realpath(args.binary)
    if not os.path.isabs(args.binary) or not os.path.isfile(binary) or not os.access(binary, os.X_OK):
        parser.error('binary must be an absolute executable file')
    instance = ROOT / 'instances' / ('nexus-wakeup-' + secrets.token_hex(12))
    with open(binary, 'rb') as executable:
        digest = hashlib.sha256()
        for block in iter(lambda: executable.read(1024 * 1024), b''):
            digest.update(block)
    report = {'schema': 'nexus.tracefs-wakeup/1', 'instance': str(instance), 'user': args.user,
              'uid': account.pw_uid, 'gid': account.pw_gid, 'binary': binary,
              'binary_sha256': digest.hexdigest(), 'frame_marker': 'composer (fixed)',
              'platform': platform.uname()._asdict(), 'samples': [],
              'scope': 'sched_wakeup events for initial TIDs only; not system wakeups or all short-lived threads'}
    created = False
    def interrupted(signum, _frame):
        raise RuntimeError(f'interrupted by signal {signum}')
    old_handlers = {sig: signal.signal(sig, interrupted) for sig in (signal.SIGINT, signal.SIGTERM)}
    try:
        verify_tracefs()
        instance.mkdir()
        created = True
        write(instance / 'tracing_on', '0')
        write(instance / 'events/enable', '0')
        for _ in range(args.runs):
            item = sample(instance, binary, account, 3.0, 15.0)
            report['samples'].append(item)
            if item['status'] != 'ok':
                break
    except (OSError, RuntimeError) as exc:
        report['error'] = str(exc)
    finally:
        for sig in old_handlers:
            signal.signal(sig, signal.SIG_IGN)
        if created:
            try:
                instance.rmdir()
                report['instance_removed'] = True
            except OSError as exc:
                report['instance_removed'] = False
                report['cleanup_error'] = str(exc)
        else:
            report['instance_removed'] = None
        for sig, handler in old_handlers.items():
            signal.signal(sig, handler)
    print(json.dumps(report, indent=2))
    return 0 if report['instance_removed'] and len(report['samples']) == args.runs and all(s['status'] == 'ok' for s in report['samples']) else 1


if __name__ == '__main__':
    sys.exit(main())
