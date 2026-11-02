"""Observe environment lookups, and on macOS file/network/process calls, at startup.

A small preloaded shim records the *name* passed to every libc `getenv` call
made through the dynamic linker (macOS `__interpose`, Linux `LD_PRELOAD`).
Values are never read or recorded. The fixture asserts that startup does not
look up the selected provider's credential variable, then submits one task
to a loopback listener and requires that lookup to appear (positive control).
An unselected provider's credential variable must never be looked up.

On macOS, where `strace` is unavailable and DTrace needs privileges, the shim
also records `open`/`openat` paths, `connect` address families, and
`posix_spawn`/`fork`/`execve` calls. Startup must open the configuration (a
sanity check that the hooks are active) but must not open `credentials.json`,
connect to an internet address, or create a process; the submission must
connect (positive control). Linux keeps its separate `strace` evidence.

Limits: calls that bypass these libc entry points (direct `environ` access,
raw syscalls, statically linked code, calls internal to one system library)
are not observed. Rust std reads variables through `getenv`; the workspace
has no `std::env::vars` enumeration. The shim must observe known startup
lookups (`HOME`, `NEXUS_CONFIG`) or the fixture fails rather than reporting an
unobserved run as clean.
"""

from contextlib import ExitStack
import json
import os
from pathlib import Path
import pty
import fcntl
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "tools" / "perf"))
from readiness import ProbeScreen

SHIM = r"""
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int log_fd = -1;
static int event_fd = -1;

#ifdef __APPLE__
#define REAL_GETENV getenv
#else
static char *(*real_getenv)(const char *);
#define REAL_GETENV real_getenv
#endif

__attribute__((constructor)) static void nexus_getenv_init(void) {
#ifndef __APPLE__
    real_getenv = (char *(*)(const char *))dlsym(RTLD_NEXT, "getenv");
#endif
    const char *path = REAL_GETENV ? REAL_GETENV("NEXUS_GETENV_LOG") : NULL;
    if (path != NULL) {
        log_fd = open(path, O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0600);
    }
#ifdef __APPLE__
    path = getenv("NEXUS_EVENT_LOG");
    if (path != NULL) {
        event_fd = open(path, O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0600);
    }
#endif
}

static void nexus_record(const char *name) {
    char line[256];
    size_t length;
    if (log_fd < 0 || name == NULL) {
        return;
    }
    length = strnlen(name, sizeof(line) - 2);
    memcpy(line, name, length);
    line[length] = '\n';
    (void)write(log_fd, line, length + 1);
}

#ifdef __APPLE__
#include <spawn.h>
#include <stdarg.h>
#include <sys/socket.h>

static void nexus_event(const char *kind, const char *detail) {
    char line[1200];
    size_t used = 0;
    size_t kind_len, detail_len;
    if (event_fd < 0) {
        return;
    }
    kind_len = strnlen(kind, 32);
    detail_len = detail ? strnlen(detail, sizeof(line) - kind_len - 3) : 0;
    memcpy(line, kind, kind_len);
    used = kind_len;
    line[used++] = ' ';
    if (detail_len) {
        memcpy(line + used, detail, detail_len);
        used += detail_len;
    }
    line[used++] = '\n';
    (void)write(event_fd, line, used);
}

static int nexus_open(const char *path, int flags, ...) {
    int mode = 0;
    if (flags & O_CREAT) {
        va_list args;
        va_start(args, flags);
        mode = va_arg(args, int);
        va_end(args);
    }
    nexus_event("open", path);
    return open(path, flags, mode);
}

static int nexus_openat(int dirfd, const char *path, int flags, ...) {
    int mode = 0;
    if (flags & O_CREAT) {
        va_list args;
        va_start(args, flags);
        mode = va_arg(args, int);
        va_end(args);
    }
    nexus_event("open", path);
    return openat(dirfd, path, flags, mode);
}

static int nexus_connect(int fd, const struct sockaddr *address, socklen_t length) {
    const char *family = "other";
    if (address != NULL && address->sa_family == AF_INET) family = "inet";
    if (address != NULL && address->sa_family == AF_INET6) family = "inet6";
    if (address != NULL && address->sa_family == AF_UNIX) family = "unix";
    nexus_event("connect", family);
    return connect(fd, address, length);
}

static int nexus_posix_spawn(pid_t *pid, const char *path,
                             const posix_spawn_file_actions_t *actions,
                             const posix_spawnattr_t *attributes,
                             char *const argv[], char *const envp[]) {
    nexus_event("spawn", path);
    return posix_spawn(pid, path, actions, attributes, argv, envp);
}

static int nexus_posix_spawnp(pid_t *pid, const char *file,
                              const posix_spawn_file_actions_t *actions,
                              const posix_spawnattr_t *attributes,
                              char *const argv[], char *const envp[]) {
    nexus_event("spawn", file);
    return posix_spawnp(pid, file, actions, attributes, argv, envp);
}

static pid_t nexus_fork(void) {
    nexus_event("fork", NULL);
    return fork();
}

static int nexus_execve(const char *path, char *const argv[], char *const envp[]) {
    nexus_event("exec", path);
    return execve(path, argv, envp);
}

static char *nexus_getenv(const char *name) {
    nexus_record(name);
    return getenv(name);
}
__attribute__((used)) static struct {
    const void *replacement;
    const void *replacee;
} nexus_interposers[] __attribute__((section("__DATA,__interpose"))) = {
    {(const void *)nexus_getenv, (const void *)getenv},
    {(const void *)nexus_open, (const void *)open},
    {(const void *)nexus_openat, (const void *)openat},
    {(const void *)nexus_connect, (const void *)connect},
    {(const void *)nexus_posix_spawn, (const void *)posix_spawn},
    {(const void *)nexus_posix_spawnp, (const void *)posix_spawnp},
    {(const void *)nexus_fork, (const void *)fork},
    {(const void *)nexus_execve, (const void *)execve},
};
#else
char *getenv(const char *name) {
    if (real_getenv == NULL) {
        real_getenv = (char *(*)(const char *))dlsym(RTLD_NEXT, "getenv");
    }
    nexus_record(name);
    return real_getenv ? real_getenv(name) : NULL;
}
#endif
"""

SELECTED = "NEXUS_TEST_SELECTED_KEY"
UNSELECTED = "NEXUS_TEST_UNSELECTED_KEY"


def stop_target(proc):
    if proc.poll() is not None:
        return
    os.killpg(proc.pid, signal.SIGTERM)
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait(timeout=5)


def lookups(path):
    try:
        return Path(path).read_text(encoding="utf-8", errors="replace").splitlines()
    except FileNotFoundError:
        return []


def main(binary):
    with ExitStack() as cleanup:
        root = cleanup.enter_context(tempfile.TemporaryDirectory(prefix="nexus-credential-env-"))
        shim = os.path.join(root, "getenv-shim.dylib" if sys.platform == "darwin" else "getenv-shim.so")
        source = os.path.join(root, "getenv-shim.c")
        Path(source).write_text(SHIM)
        flags = ["-dynamiclib"] if sys.platform == "darwin" else ["-shared", "-fPIC"]
        libs = [] if sys.platform == "darwin" else ["-ldl"]
        subprocess.run(["cc", *flags, "-O2", "-o", shim, source, *libs], check=True)
        log = os.path.join(root, "getenv.log")
        events = os.path.join(root, "events.log")

        listener = socket.socket()
        cleanup.callback(listener.close)
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        endpoint = "http://127.0.0.1:%d/v1" % listener.getsockname()[1]
        config = {
            "revision": 2,
            "providers": [
                {"id": "p", "display_name": "Selected", "adapter": "direct",
                 "protocol": "chat_completions", "endpoint": endpoint,
                 "credential": {"env": SELECTED}, "default_model": "model"},
                {"id": "q", "display_name": "Unselected", "adapter": "direct",
                 "protocol": "chat_completions", "endpoint": endpoint,
                 "credential": {"env": UNSELECTED}, "default_model": "model"},
            ],
            "models": [{"id": "m", "provider": "p", "name": "model"},
                       {"id": "n", "provider": "q", "name": "model"}],
            "favourites": [], "recent": [],
        }
        config_path = Path(root) / "config.json"
        config_path.write_text(json.dumps(config))
        env = {"HOME": root, "XDG_CONFIG_HOME": root + "/config",
               "XDG_STATE_HOME": root + "/state", "XDG_DATA_HOME": root + "/data",
               "XDG_CACHE_HOME": root + "/cache", "NEXUS_CONFIG": str(config_path),
               "TERM": "xterm-256color", SELECTED: "synthetic-selected",
               UNSELECTED: "synthetic-unselected", "NEXUS_GETENV_LOG": log,
               "NEXUS_EVENT_LOG": events,
               "PATH": os.environ.get("PATH", "/usr/bin:/bin")}
        env["DYLD_INSERT_LIBRARIES" if sys.platform == "darwin" else "LD_PRELOAD"] = shim

        master, slave = pty.openpty()
        cleanup.callback(os.close, master)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        proc = subprocess.Popen([binary], stdin=slave, stdout=slave, stderr=slave,
                                env=env, cwd=root, start_new_session=True)
        cleanup.callback(stop_target, proc)
        os.close(slave)
        screen = ProbeScreen(120, 40)
        captured = bytearray()

        def pump(seconds, until=None, label=""):
            deadline = time.monotonic() + seconds
            while True:
                if until is not None and until():
                    return True
                if time.monotonic() >= deadline:
                    assert until is None, (label, captured[-2000:])
                    return False
                assert proc.poll() is None, (label, proc.returncode, captured[-2000:])
                ready, _, _ = select.select([master, listener], [], [], 0.05)
                if master in ready:
                    data = os.read(master, 65536)
                    screen.feed(data)
                    captured.extend(data)
                    del captured[:-65536]
                if listener in ready:
                    return "connected"

        assert pump(15, lambda: screen.contains("composer (fixed)"), "first frame")
        # Remain idle after the first frame so deferred startup work can run.
        assert pump(1) != "connected", "startup contacted the provider"
        startup = lookups(log)
        assert "HOME" in startup and "NEXUS_CONFIG" in startup, (
            "getenv shim did not observe known startup lookups", sorted(set(startup)))
        assert SELECTED not in startup, "startup looked up the selected credential"
        assert UNSELECTED not in startup, "startup looked up an unselected credential"
        startup_events = lookups(events)
        if sys.platform == "darwin":
            opened = [line[5:] for line in startup_events if line.startswith("open ")]
            assert str(config_path) in opened, ("event shim did not observe config open", opened)
            assert not any(path.endswith("credentials.json") for path in opened), opened
            network = [line for line in startup_events
                       if line in ("connect inet", "connect inet6")]
            assert not network, ("startup network connect", network)
            processes = [line for line in startup_events
                         if line.startswith(("spawn ", "exec ")) or line == "fork "]
            assert not processes, ("startup process creation", processes)

        os.write(master, b"credential positive control\r")
        assert pump(10, None, "submission") == "connected", "submission did not reach the provider"
        submitted = lookups(log)
        assert SELECTED in submitted, "submission did not look up the selected credential"
        assert UNSELECTED not in submitted, "submission looked up an unselected credential"
        submitted_events = lookups(events)
        if sys.platform == "darwin":
            assert "connect inet" in submitted_events, "event shim missed the provider connect"
            assert not any(line.startswith("open ") and line.endswith("credentials.json")
                           for line in submitted_events)
        print(json.dumps({
            "platform": sys.platform,
            "startup_lookup_names": sorted(set(startup)),
            "selected_credential_looked_up_at_startup": False,
            "selected_credential_looked_up_after_submit": True,
            "unselected_credential_looked_up": False,
            "startup_events_observed": len(startup_events),
            "startup_opened_under_fixture": sorted({
                os.path.relpath(line[5:], root) for line in startup_events
                if line.startswith("open ") and line[5:].startswith(root)}),
            "startup_inet_connects": 0 if sys.platform == "darwin" else None,
            "startup_process_creations": 0 if sys.platform == "darwin" else None,
        }), flush=True)


if __name__ == "__main__":
    main(sys.argv[1])
