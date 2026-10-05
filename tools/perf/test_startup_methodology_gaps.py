#!/usr/bin/env python3
"""Gap tests for methodology/provenance recording in tools/perf/startup.py.

test_startup.py owns timing math, sample classification, purge invalidation,
and the happy-path `--json` record. This file pins only the
methodology/provenance honesty contract:

- the target binary's SHA-256 is recorded and is derived from file content,
  so a record identifies the exact artifact that was measured;
- every cold run carries exactly one of the four `cache_prep` labels
  (pending / success / failed / none) and none of them claims a reset;
- `cache_reset_verified` is `false` in every configuration the CLI accepts;
- provenance collected outside a git repository is null-safe: no exception,
  every documented key present, JSON-serializable without NaN, exit status
  unaffected;
- no watcher thread outlives a launch, a timeout, a purge run, or a batch.

Deterministic and offline: targets are the local interpreter or a small
temporary script, cache-prep commands are POSIX shell builtins, and no
git remote, network call, or privileged command is involved. The only
processes spawned are this harness, its own targets, and `git`/`sysctl`
probes that startup.py already invokes best-effort.

Run from the repository root:
    python3 -m unittest discover -s tools/perf -p 'test_*.py'
or directly:
    python3 tools/perf/test_startup_methodology_gaps.py
"""

import contextlib
import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import startup  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "startup.py")
POSIX_SHELL = os.name == "posix"
BENIGN = [sys.executable, "-c", "pass"]
SLEEPER = [sys.executable, "-c", "import time; time.sleep(30)"]

# The only cache_prep labels startup.py may emit for a cold-mode entry.
LABEL_PENDING = "purge configured; result pending (cache reset not verified)"
LABEL_SUCCESS = ("user-supplied purge command exited 0 for every sample "
                 "(cache reset not verified)")
LABEL_FAILED = "purge failed; cache reset not verified"
LABEL_NONE = "none"
PREP_LABELS = frozenset({LABEL_PENDING, LABEL_SUCCESS, LABEL_FAILED, LABEL_NONE})

PROVENANCE_KEYS = ("repo_commit", "repo_dirty", "lockfile", "lockfile_sha256")

# Generous ceiling for one CLI run; every inner --timeout is far smaller, so a
# hang fails inside the harness instead of exhausting this budget.
CLI_TIMEOUT = 120

# startup.py names its wait/reap watcher thread this; a retained thread with
# this name means the batch left a waiter behind.
WATCHER_THREAD = "startup-wait"


def watcher_threads():
    """Live watcher threads the harness has not finished with."""
    return [t for t in threading.enumerate() if t.name == WATCHER_THREAD]


def wait_for_watchers(count, timeout=5.0):
    """Poll until `count` watcher threads are live; return what is live."""
    deadline = time.monotonic() + timeout
    current = watcher_threads()
    while len(current) != count and time.monotonic() < deadline:
        time.sleep(0.02)
        current = watcher_threads()
    return current


def write_bytes_file(path, payload):
    with open(path, "wb") as handle:
        handle.write(payload)
    return path


def write_script_file(path, payload=b""):
    """Write an *executable* target that startup.py can actually launch.

    Popen execs the binary itself, so a plain data file is not a runnable
    target: the harness records it as a `launch_error` invalid sample and
    exits 1. The shebang is fixed boilerplate and `payload` stays separate so
    content-hash expectations remain readable.
    """
    path = write_bytes_file(path, b"#!/usr/bin/env python3\n" + payload)
    os.chmod(path, 0o755)
    return path


def read_text(path):
    with open(path, encoding="utf-8") as handle:
        return handle.read()


def load_record(path):
    """Return (raw_text, record) from a `--json` file; strict JSON only.

    Python's decoder accepts the bare NaN/Infinity literals json.dump emits by
    default, so parse_constant rejects them instead of silently passing a
    record that is not valid JSON.
    """
    text = read_text(path)

    def reject(constant):
        raise AssertionError(f"record contains non-finite literal {constant!r}")

    return text, json.loads(text, parse_constant=reject)


def sha256_of(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


@contextlib.contextmanager
def working_directory(path):
    """chdir(path) for the block; yields the physical working directory.

    startup.py builds paths from os.getcwd(), which resolves symlinks (on
    macOS TMPDIR is a symlink into /private/var), so callers comparing
    recorded paths must use the resolved form.
    """
    previous = os.getcwd()
    os.chdir(path)
    try:
        yield os.getcwd()
    finally:
        os.chdir(previous)


@contextlib.contextmanager
def patched(module, name, value):
    original = getattr(module, name)
    setattr(module, name, value)
    try:
        yield value
    finally:
        setattr(module, name, original)


def stubbed_run_batch(recorder=None):
    """Deterministic stand-in for run_batch that launches nothing.

    `recorder` is invoked from inside main's frame while a mode entry is
    still in flight, which is the only point at which the "pending"
    cache_prep label is observable; every sample it returns is `ok`.
    """
    def fake_run_batch(argv, runs, timeout, purge_cmd=None):
        if recorder is not None:
            entry = sys._getframe(1).f_locals.get("entry")
            recorder(dict(entry) if isinstance(entry, dict) else {}, purge_cmd)
        return [
            {"index": index, "outcome": startup.OK, "elapsed_ms": float(index + 1),
             "max_rss": None, "exit_code": 0, "signal": None, "error": None}
            for index in range(runs)
        ]
    return fake_run_batch


def run_main_in_process(argv):
    """Run startup.main(argv) with stdout captured; return (code, stdout)."""
    buffer = io.StringIO()
    with contextlib.redirect_stdout(buffer):
        code = startup.main(argv)
    return code, buffer.getvalue()


class StartupGapTestCase(unittest.TestCase):
    """Shared helpers: a non-repo working directory and the CLI runner."""

    def non_repo_dir(self):
        """A temporary directory that is not inside a git repository."""
        path = tempfile.mkdtemp(prefix="nexus-perf-nonrepo-")
        self.addCleanup(shutil.rmtree, path, True)
        self.assertFalse(
            os.path.exists(os.path.join(path, ".git")),
            f"non-repo provenance tests need a directory outside a repo: {path}",
        )
        return path

    def run_cli(self, *args, cwd=None, timeout=CLI_TIMEOUT):
        proc = subprocess.run(
            [sys.executable, SCRIPT, *args],
            capture_output=True, text=True, timeout=timeout, cwd=cwd,
        )
        # An unhandled harness crash is a bug in any exit status we assert.
        self.assertNotIn("Traceback", proc.stderr, proc.stderr)
        return proc


class BinarySha256ProvenanceTests(StartupGapTestCase):
    def test_recorded_sha256_is_derived_from_binary_content(self):
        with tempfile.TemporaryDirectory() as tmp:
            measured = write_script_file(os.path.join(tmp, "target-a.py"),
                                         b"# nexus perf target a\n")
            other = write_bytes_file(os.path.join(tmp, "target-b.py"),
                                     b"# nexus perf target b\n")
            out = os.path.join(tmp, "perf.json")
            proc = self.run_cli(measured, "-n", "1", "--json", out,
                                cwd=self.non_repo_dir())
            self.assertEqual(proc.returncode, 0, proc.stderr)
            text, record = load_record(out)
            method = record["methodology"]
            self.assertEqual(method["binary"], measured)
            self.assertEqual(method["binary_bytes"], os.path.getsize(measured))
            self.assertEqual(method["binary_sha256"], sha256_of(measured))
            self.assertEqual(len(method["binary_sha256"]), 64)
            # Content-derived, not name- or path-derived: a sibling file with
            # different bytes hashes differently.
            self.assertNotEqual(method["binary_sha256"], sha256_of(other))
            self.assertNotIn("NaN", text)
            # The same digest is printed in the human-readable header.
            self.assertIn(f"  binary_sha256: {sha256_of(measured)}", proc.stdout)

    def test_lockfile_provenance_is_recorded_when_cargo_lock_exists(self):
        lock_payload = b"# unit-test lock\nversion = 4\n"
        with tempfile.TemporaryDirectory() as tmp:
            write_bytes_file(os.path.join(tmp, "Cargo.lock"), lock_payload)
            binary = write_bytes_file(os.path.join(tmp, "target.py"), b"pass\n")
            with working_directory(tmp) as cwd:
                info = startup.collect_environment(binary, "unit-test/gaps", None)
                expected_lock = os.path.join(cwd, "Cargo.lock")
            self.assertEqual(info["lockfile"], expected_lock)
            self.assertEqual(info["lockfile_sha256"],
                             hashlib.sha256(lock_payload).hexdigest())
            self.assertEqual(info["build_profile"], "unit-test/gaps")
            self.assertEqual(info["binary_sha256"], sha256_of(binary))
            self.assertEqual(info["cold_purge_cmd"], "none (cache not purged)")
            self.assertTrue(info["timestamp_utc"].endswith("+00:00"))

    def test_unresolvable_binary_path_records_unknown_instead_of_raising(self):
        missing = os.path.join(self.non_repo_dir(), "nexus-perf-absent-binary")
        info = startup.collect_environment(missing, "unit-test/gaps", None)
        self.assertEqual(info["binary_sha256"], "unknown")
        self.assertEqual(info["binary_bytes"], "unknown")


class ColdCachePrepLabelTests(StartupGapTestCase):
    def test_pending_label_is_recorded_while_a_purge_batch_is_in_flight(self):
        observed = []
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            argv = [sys.executable, "-n", "2", "--mode", "cold", "--timeout", "30",
                    "--purge-cmd", "true", "--json", out]
            with patched(startup, "run_batch", stubbed_run_batch(
                    lambda entry, purge: observed.append((entry, purge)))):
                code, stdout = run_main_in_process(argv)
            _, record = load_record(out)
        self.assertEqual(code, 0)
        self.assertEqual(len(observed), 1)
        in_flight, purge_cmd = observed[0]
        self.assertEqual(purge_cmd, "true")
        self.assertEqual(in_flight["cache_prep"], LABEL_PENDING)
        self.assertIn("cache reset not verified", in_flight["cache_prep"])
        # Pending means "not yet known": no results and no purge attempts so far.
        self.assertIsNone(in_flight["results"])
        self.assertEqual(in_flight["purge_runs"], 0)
        self.assertTrue(in_flight["valid"])
        # A clean batch replaces pending with the success label.
        entry = record["modes"][0]
        self.assertEqual(entry["cache_prep"], LABEL_SUCCESS)
        self.assertEqual(entry["purge_runs"], 2)
        self.assertEqual(entry["results"]["successful"], 2)
        self.assertNotIn("pending", json.dumps(record))
        self.assertIn("cannot verify", stdout)

    @unittest.skipUnless(POSIX_SHELL, "cache-prep command uses a POSIX shell")
    def test_failed_label_and_evidence_when_purge_exits_nonzero(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self.run_cli(
                sys.executable, "-n", "2", "--mode", "cold", "--purge-cmd", "exit 9",
                "--json", out, "--", "-c", "pass",
            )
            self.assertEqual(proc.returncode, 2, proc.stderr)
            _, record = load_record(out)
            entry = record["modes"][0]
        self.assertEqual(entry["cache_prep"], LABEL_FAILED)
        self.assertFalse(entry["valid"])
        self.assertIn("cache reset not verified", entry["cache_prep"])
        self.assertEqual(entry["purge_error"]["reason"], "nonzero")
        self.assertEqual(entry["purge_error"]["exit_code"], 9)
        self.assertEqual(entry["purge_error"]["failed_at_sample"], 0)
        self.assertEqual(entry["purge_runs"], 1)
        self.assertEqual(entry["samples"], [])
        self.assertIsNone(entry["results"])

    @unittest.skipUnless(POSIX_SHELL, "cache-prep command uses a POSIX shell")
    def test_failed_label_when_purge_times_out(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            started = time.monotonic()
            proc = self.run_cli(
                sys.executable, "-n", "1", "--mode", "cold", "--purge-cmd", "sleep 30",
                "--timeout", "0.5", "--json", out, "--", "-c", "pass",
            )
            wall = time.monotonic() - started
            self.assertEqual(proc.returncode, 2, proc.stderr)
            _, record = load_record(out)
            entry = record["modes"][0]
        self.assertLess(wall, 15.0)
        self.assertEqual(entry["cache_prep"], LABEL_FAILED)
        self.assertFalse(entry["valid"])
        self.assertEqual(entry["purge_error"]["reason"], "timeout")
        self.assertIn("termination requested", entry["purge_error"]["detail"])
        self.assertEqual(entry["purge_error"]["failed_at_sample"], 0)
        self.assertEqual(entry["purge_runs"], 1)
        self.assertIsNone(entry["results"])

    def test_none_label_when_cold_is_requested_without_a_purge_command(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self.run_cli(
                sys.executable, "-n", "1", "--mode", "cold", "--json", out,
                "--", "-c", "pass",
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            _, record = load_record(out)
        entry = record["modes"][0]
        self.assertEqual(entry["cache_prep"], LABEL_NONE)
        self.assertIsNone(entry["purge_cmd"])
        self.assertEqual(entry["purge_runs"], 0)
        self.assertNotIn("purge_error", entry)
        # Two distinct keys: cold_purge_cmd reports the (absent) command, while
        # cold_cache_prep carries the "unprepared" caveat about the samples.
        self.assertEqual(record["methodology"]["cold_purge_cmd"],
                         "none (cache not purged)")
        self.assertEqual(record["methodology"]["cold_cache_prep"],
                         "none (cache not purged; cold runs are unprepared)")
        self.assertFalse(record["methodology"]["cache_reset_verified"])
        self.assertIn("OS cache was NOT reset (unprepared-cache runs)", proc.stdout)

    @unittest.skipUnless(POSIX_SHELL, "cache-prep command uses a POSIX shell")
    def test_every_mode_entry_carries_one_of_the_four_prep_labels(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self.run_cli(
                sys.executable, "-n", "1", "--mode", "both", "--purge-cmd", "true",
                "--timeout", "30", "--json", out, "--", "-c", "pass",
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            _, record = load_record(out)
        self.assertEqual([entry["mode"] for entry in record["modes"]],
                         ["warm", "cold"])
        for entry in record["modes"]:
            with self.subTest(mode=entry["mode"]):
                self.assertIn(entry["cache_prep"], PREP_LABELS)
        # --mode both with a purge command: warm never purges, cold does.
        self.assertEqual([entry["cache_prep"] for entry in record["modes"]],
                         [LABEL_NONE, LABEL_SUCCESS])


class CacheResetVerifiedTests(StartupGapTestCase):
    CONFIGURATIONS = (
        ("warm", None),
        ("cold", None),
        ("cold", "true"),
        ("both", "true"),
    )

    def test_cache_reset_verified_is_false_for_every_configuration(self):
        for mode, purge_cmd in self.CONFIGURATIONS:
            with self.subTest(mode=mode, purge_cmd=purge_cmd):
                with tempfile.TemporaryDirectory() as tmp:
                    out = os.path.join(tmp, "perf.json")
                    argv = [sys.executable, "-n", "2", "--mode", mode,
                            "--timeout", "30", "--json", out]
                    if purge_cmd:
                        argv += ["--purge-cmd", purge_cmd]
                    with patched(startup, "run_batch", stubbed_run_batch()):
                        code, _ = run_main_in_process(argv)
                    text, record = load_record(out)
                self.assertEqual(code, 0)
                method = record["methodology"]
                # A real boolean false, hardcoded rather than derived from a
                # purge outcome: no CLI input can flip it.
                self.assertIsInstance(method["cache_reset_verified"], bool)
                self.assertIs(method["cache_reset_verified"], False)
                self.assertIn('"cache_reset_verified": false', text)
                self.assertNotIn("NaN", text)
                if purge_cmd:
                    self.assertIn("cache reset not verified",
                                  method["cold_cache_prep"])
                else:
                    self.assertEqual(method["cold_purge_cmd"],
                                     "none (cache not purged)")


class NonRepoProvenanceTests(StartupGapTestCase):
    def test_cli_record_outside_a_repo_keeps_null_provenance_and_exits_zero(self):
        with tempfile.TemporaryDirectory() as tmp:
            binary = write_script_file(os.path.join(tmp, "target.py"),
                                      b"# non-repo target\n")
            out = os.path.join(tmp, "perf.json")
            # Digest the target while it still exists: the temporary
            # directory is removed on leaving the block below.
            expected_sha256 = sha256_of(binary)
            proc = self.run_cli(binary, "-n", "1", "--json", out,
                                cwd=self.non_repo_dir())
            self.assertEqual(proc.returncode, 0, proc.stderr)
            text, record = load_record(out)
            method = record["methodology"]
        for key in PROVENANCE_KEYS:
            with self.subTest(key=key):
                self.assertIn(key, method)
                self.assertIsNone(method[key])
        self.assertNotIn("NaN", text)
        # Provenance gaps do not degrade the artifact identity or the run.
        self.assertEqual(method["binary_sha256"], expected_sha256)
        self.assertEqual(len(method["binary_sha256"]), 64)

    def test_methodology_without_a_repo_serializes_without_nonfinite_floats(self):
        with working_directory(self.non_repo_dir()):
            info = startup.collect_environment(BENIGN[0], "unit-test/gaps", None)
            # allow_nan=False rejects NaN/Infinity outright.
            text = json.dumps(info, allow_nan=False, sort_keys=True)
        round_tripped = json.loads(text)
        self.assertEqual(round_tripped, info)
        for key in PROVENANCE_KEYS:
            with self.subTest(key=key):
                self.assertIsNone(round_tripped[key])


class WatcherThreadTests(unittest.TestCase):
    def assert_no_watcher_threads(self, timeout=5.0):
        live = wait_for_watchers(0, timeout=timeout)
        self.assertEqual([t.name for t in live], [],
                         "a watcher thread outlived the operation that started it")

    def test_successful_launch_leaves_no_watcher_thread(self):
        sample = startup.measure_once(BENIGN, timeout=30)
        self.assertEqual(sample["outcome"], startup.OK, sample)
        self.assert_no_watcher_threads()

    def test_timed_out_launch_leaves_no_watcher_thread(self):
        started = time.monotonic()
        sample = startup.measure_once(SLEEPER, timeout=0.3)
        self.assertEqual(sample["outcome"], "timeout", sample)
        self.assertLess(time.monotonic() - started, 10.0)
        self.assert_no_watcher_threads()

    @unittest.skipUnless(POSIX_SHELL, "cache-prep command uses a POSIX shell")
    def test_purge_success_and_purge_timeout_leave_no_watcher_thread(self):
        evidence = startup.run_purge("true", timeout=30)
        self.assertEqual(evidence["exit_code"], 0, evidence)
        self.assertIsNone(evidence["signal"])
        self.assert_no_watcher_threads()
        with self.assertRaises(startup.PurgeError) as ctx:
            startup.run_purge("sleep 30", timeout=0.3)
        self.assertEqual(ctx.exception.reason, "timeout")
        self.assert_no_watcher_threads()

    @unittest.skipUnless(POSIX_SHELL, "cache-prep command uses a POSIX shell")
    def test_batch_with_purge_leaves_no_watcher_thread(self):
        samples = startup.run_batch(BENIGN, 2, timeout=30, purge_cmd="true")
        self.assertEqual([s["outcome"] for s in samples], ["ok", "ok"])
        self.assert_no_watcher_threads()

    def test_watcher_exists_only_while_a_target_is_in_flight(self):
        # Positive control: the negative assertions above would be vacuous if
        # no watcher were ever created, and a blocking waiter that is not a
        # daemon could delay interpreter shutdown.
        holder = {}

        def launch():
            holder["sample"] = startup.measure_once(
                [sys.executable, "-c", "import time; time.sleep(2.0)"], timeout=30
            )

        thread = threading.Thread(target=launch, name="gap-launch")
        thread.start()
        try:
            in_flight = wait_for_watchers(1, timeout=15.0)
            self.assertEqual(len(in_flight), 1,
                             "no watcher thread observed while the target ran")
            self.assertTrue(in_flight[0].daemon,
                            "the wait/reap watcher must be a daemon thread")
        finally:
            thread.join(timeout=30)
        self.assertFalse(thread.is_alive(), "launch thread did not finish")
        self.assertEqual(holder["sample"]["outcome"], startup.OK)
        self.assert_no_watcher_threads()


if __name__ == "__main__":
    unittest.main(verbosity=2)