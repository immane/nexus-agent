#!/usr/bin/env python3
"""Gap tests for the tools/perf/startup.py CLI contract and --json record.

Complements test_startup.py, which covers the library helpers and sample
classification directly. This file exercises only the observable end-to-end
contract of the CLI, through bounded subprocess runs:

- the JSON record shape: schema tag, methodology fields, mode entries, raw
  samples, and summary results;
- strict-JSON output with no non-finite literals (no bare NaN/Infinity);
- exit status and retained evidence for a batch in which every launch fails;
- CLI rejection of unusable timeouts and run counts before anything is
  measured or written;
- the warm/cold/both mode records, including unprepared cold runs and a cold
  mode invalidated by a purge failure.

Every launch is `python -c ...`, every purge command is a shell builtin or a
local marker file, and every subprocess call has an explicit timeout. Nothing
here touches the network, and no test asserts a timing value, so the suite is
deterministic apart from the machine-dependent measurements it deliberately
leaves unconstrained.

Run from the repository root:
    python3 -m unittest discover -s tools/perf -p 'test_*.py'
or directly:
    python3 tools/perf/test_startup_cli_gaps.py
"""

import hashlib
import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
import unittest
from datetime import datetime, timedelta, timezone

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import startup  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "startup.py")
REPO_ROOT = os.path.dirname(os.path.dirname(HERE))
POSIX_WAIT4 = hasattr(os, "wait4")
POSIX_SHELL = os.name == "posix"

# Generous ceiling for a whole CLI run; every inner --timeout is much smaller,
# so a hang fails fast inside the harness rather than exhausting this budget.
CLI_TIMEOUT = 120
LAUNCH_TIMEOUT = "20"

# Documented record shape (see README.md "JSON record"). Kept as explicit
# literals so a missing or renamed field fails the test instead of silently
# weakening it.
EXPECTED_METHODOLOGY_FIELDS = frozenset({
    "arch", "binary", "binary_bytes", "binary_sha256", "build_profile",
    "cache_reset_verified", "cold_cache_prep", "cold_purge_cmd", "hardware",
    "launch_argv", "lockfile", "lockfile_sha256", "mode", "os", "processor",
    "purge_timeout_s", "python", "repo_commit", "repo_dirty", "rss_method",
    "runs_per_mode", "timeout_s", "timer", "timestamp_utc", "wait_notification",
})
EXPECTED_MODE_FIELDS = frozenset({
    "mode", "valid", "invalid_reason", "cache_prep", "purge_cmd", "purge_runs",
    "samples", "results",
})
EXPECTED_SAMPLE_FIELDS = frozenset({
    "outcome", "elapsed_ms", "max_rss", "exit_code", "signal", "error", "index",
})
EXPECTED_RESULT_FIELDS = frozenset({
    "n", "p50_ms", "p95_ms", "max_ms", "attempted", "successful", "invalid",
    "max_rss",
})
NONFINITE_WORD = re.compile(r"(?i)\b(?:nan|inf|infinity)\b")
ELAPSED_MS = re.compile(r"elapsed=[0-9]+\.[0-9]{3}ms")


class CliContractTestCase(unittest.TestCase):
    """Bounded subprocess helpers shared by the CLI gap tests."""

    def run_cli(self, *args, cwd=None, timeout=CLI_TIMEOUT):
        proc = subprocess.run(
            [sys.executable, SCRIPT, *(str(arg) for arg in args)],
            capture_output=True, text=True, timeout=timeout, cwd=cwd,
        )
        # An unhandled harness crash is a bug in any exit status we assert.
        self.assertNotIn("Traceback", proc.stderr, proc.stderr)
        return proc

    def sha256_of(self, path):
        digest = hashlib.sha256()
        with open(path, "rb") as fh:
            for chunk in iter(lambda: fh.read(1024 * 1024), b""):
                digest.update(chunk)
        return digest.hexdigest()

    def load_record(self, path):
        """Return (raw_text, record); strict JSON only.

        Python's decoder accepts the bare NaN/Infinity literals that json.dump
        emits by default, so parse_constant is used to reject them.
        """
        with open(path, encoding="utf-8") as fh:
            text = fh.read()

        def reject(constant):
            raise AssertionError(f"record contains non-finite literal {constant!r}")

        return text, json.loads(text, parse_constant=reject)

    def stdout_methodology(self, proc):
        """Parse the human-readable methodology block printed before results."""
        lines = proc.stdout.splitlines()
        self.assertEqual(lines[0] if lines else "", "methodology:", proc.stdout)
        fields = {}
        for line in lines[1:]:
            if not line.startswith("  "):
                break
            key, sep, value = line.strip().partition(": ")
            self.assertNotEqual(sep, "", f"malformed methodology line: {line!r}")
            fields[key] = value
        return fields


class JsonRecordSchemaTests(CliContractTestCase):
    """The record's schema tag, field sets, and methodology contents."""

    def run_benign(self, tmp, *extra, runs=2, timeout=LAUNCH_TIMEOUT):
        """One warm CLI run over a trivial target; returns (proc, json path)."""
        out = os.path.join(tmp, "perf.json")
        argv = [sys.executable, "-n", runs, "--mode", "warm", "--timeout", timeout]
        argv += list(extra) + ["--json", out, "--", "-c", "pass"]
        return self.run_cli(*argv), out

    def test_record_has_exactly_schema_methodology_and_modes(self):
        self.assertEqual(startup.SCHEMA, "nexus.perf.startup/1")
        with tempfile.TemporaryDirectory() as tmp:
            proc, out = self.run_benign(tmp, "--build-profile", "gap/test")
            self.assertEqual(proc.returncode, 0, proc.stderr)
            _, record = self.load_record(out)
            self.assertEqual(set(record), {"schema", "methodology", "modes"})
            self.assertEqual(record["schema"], startup.SCHEMA)
            self.assertIsInstance(record["methodology"], dict)
            self.assertIsInstance(record["modes"], list)
            self.assertEqual([entry["mode"] for entry in record["modes"]], ["warm"])

    def test_methodology_contains_every_documented_field(self):
        with tempfile.TemporaryDirectory() as tmp:
            proc, out = self.run_benign(tmp, "--build-profile", "gap/schema")
            self.assertEqual(proc.returncode, 0, proc.stderr)
            _, record = self.load_record(out)
            methodology = record["methodology"]
            missing = EXPECTED_METHODOLOGY_FIELDS - set(methodology)
            extra = set(methodology) - EXPECTED_METHODOLOGY_FIELDS
            self.assertEqual((missing, extra), (set(), set()), sorted(methodology))
            for key in ("os", "arch", "processor", "python", "hardware", "timer",
                        "wait_notification", "rss_method", "timestamp_utc",
                        "cold_purge_cmd", "cold_cache_prep", "build_profile"):
                with self.subTest(field=key):
                    self.assertIsInstance(methodology[key], str)
                    self.assertTrue(methodology[key].strip(), key)
            self.assertEqual(methodology["build_profile"], "gap/schema")
            self.assertEqual(methodology["runs_per_mode"], 2)
            self.assertEqual(methodology["mode"], "warm")
            self.assertFalse(methodology["cache_reset_verified"])

    def test_methodology_records_binary_facts_timer_and_rss_method(self):
        with tempfile.TemporaryDirectory() as tmp:
            proc, out = self.run_benign(tmp, runs=1, timeout="30")
            self.assertEqual(proc.returncode, 0, proc.stderr)
            _, record = self.load_record(out)
            methodology = record["methodology"]
            self.assertEqual(methodology["binary"], sys.executable)
            self.assertEqual(methodology["launch_argv"], [sys.executable, "-c", "pass"])
            self.assertEqual(methodology["binary_bytes"], os.path.getsize(sys.executable))
            self.assertEqual(
                methodology["binary_sha256"], self.sha256_of(sys.executable)
            )
            self.assertEqual(methodology["timeout_s"], 30.0)
            # One --timeout bounds both launches and purge commands.
            self.assertEqual(methodology["purge_timeout_s"], methodology["timeout_s"])
            self.assertIn("perf_counter", methodology["timer"])
            self.assertEqual(methodology["rss_method"], startup.rss_method())
            expected_wait = ("blocking os.wait4 with event watchdog" if POSIX_WAIT4
                             else "blocking Popen.wait with event watchdog")
            self.assertEqual(methodology["wait_notification"], expected_wait)
            self.assertEqual(methodology["cold_purge_cmd"], "none (cache not purged)")

    def test_timestamp_is_timezone_aware_utc_from_the_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            before = datetime.now(timezone.utc)
            proc, out = self.run_benign(tmp, runs=1)
            after = datetime.now(timezone.utc)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            _, record = self.load_record(out)
            stamp = datetime.fromisoformat(record["methodology"]["timestamp_utc"])
            self.assertIsNotNone(stamp.tzinfo, "timestamp_utc must carry a timezone")
            self.assertEqual(stamp.utcoffset(), timedelta(0))
            self.assertGreaterEqual(stamp, before - timedelta(seconds=1))
            self.assertLessEqual(stamp, after + timedelta(seconds=1))

    def test_stdout_methodology_block_mirrors_the_record(self):
        with tempfile.TemporaryDirectory() as tmp:
            proc, out = self.run_benign(tmp, "--build-profile", "gap/mirror")
            self.assertEqual(proc.returncode, 0, proc.stderr)
            _, record = self.load_record(out)
            fields = self.stdout_methodology(proc)
            self.assertEqual(set(fields), set(record["methodology"]))
            methodology = record["methodology"]
            self.assertEqual(fields["build_profile"], "gap/mirror")
            self.assertEqual(fields["binary"], methodology["binary"])
            self.assertEqual(fields["runs_per_mode"], "2")
            self.assertEqual(fields["cache_reset_verified"], str(False))

    def test_mode_entry_sample_and_result_field_sets(self):
        with tempfile.TemporaryDirectory() as tmp:
            proc, out = self.run_benign(tmp, runs=2)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            _, record = self.load_record(out)
            entry = record["modes"][0]
            self.assertEqual(set(entry), EXPECTED_MODE_FIELDS, sorted(entry))
            self.assertTrue(entry["valid"])
            self.assertIsNone(entry["invalid_reason"])
            self.assertNotIn("purge_error", entry)
            results = entry["results"]
            self.assertEqual(set(results), EXPECTED_RESULT_FIELDS, sorted(results))
            self.assertEqual([set(s) for s in entry["samples"]],
                             [EXPECTED_SAMPLE_FIELDS] * 2)
            self.assertEqual([s["index"] for s in entry["samples"]], [0, 1])
            self.assertEqual(results["n"], 2)
            self.assertEqual(results["successful"], 2)
            self.assertEqual(results["attempted"], 2)
            self.assertEqual(results["invalid"], {})
            self.assertLessEqual(results["p50_ms"], results["p95_ms"])
            self.assertLessEqual(results["p95_ms"], results["max_ms"])
            for sample in entry["samples"]:
                self.assertEqual(sample["outcome"], startup.OK)
                self.assertEqual(sample["exit_code"], 0)
                self.assertIsNone(sample["signal"])
                self.assertIsNone(sample["error"])
                self.assertGreater(sample["elapsed_ms"], 0.0)

    @unittest.skipUnless(POSIX_WAIT4, "target max RSS requires POSIX os.wait4")
    def test_successful_samples_carry_target_rss_matching_the_summary(self):
        with tempfile.TemporaryDirectory() as tmp:
            proc, out = self.run_benign(tmp, runs=2)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            _, record = self.load_record(out)
            entry = record["modes"][0]
            values = [s["max_rss"] for s in entry["samples"]]
            self.assertTrue(all(isinstance(v, int) and v > 0 for v in values), values)
            self.assertEqual(entry["results"]["max_rss"], max(values))
            self.assertIn(startup.rss_unit(), proc.stdout)

    def test_provenance_is_absent_when_cwd_has_no_repo_or_lockfile(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self.run_cli(sys.executable, "-n", "1", "--timeout", LAUNCH_TIMEOUT,
                                "--json", out, "--", "-c", "pass", cwd=tmp)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            _, record = self.load_record(out)
            provenance = record["methodology"]
            self.assertIsNone(provenance["lockfile"])
            self.assertIsNone(provenance["lockfile_sha256"])
            # Best-effort provenance stays None-valued outside a checkout and
            # is never fatal to the run.
            commit = provenance["repo_commit"]
            self.assertTrue(commit is None or re.fullmatch(r"[0-9a-f]{40}", commit),
                            f"unexpected repo_commit: {commit!r}")
            self.assertIn(provenance["repo_dirty"], (None, True, False))

    @unittest.skipUnless(os.path.isfile(os.path.join(REPO_ROOT, "Cargo.lock")),
                         "repository checkout has no Cargo.lock")
    def test_provenance_hashes_the_cwd_lockfile(self):
        lockfile = os.path.join(REPO_ROOT, "Cargo.lock")
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self.run_cli(sys.executable, "-n", "1", "--timeout", LAUNCH_TIMEOUT,
                                "--json", out, "--", "-c", "pass", cwd=REPO_ROOT)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            _, record = self.load_record(out)
            provenance = record["methodology"]
            self.assertEqual(provenance["lockfile"], lockfile)
            self.assertEqual(provenance["lockfile_sha256"], self.sha256_of(lockfile))


class NaNFreeOutputTests(CliContractTestCase):
    """No non-finite literal may reach the JSON record or the summary lines."""

    # Deliberately free of "nan"/"inf" so a stray non-finite value in the
    # record text cannot be mistaken for part of this path.
    MISSING = "/nonexistent/nexus-perf-nonfinite-target"

    def test_record_text_is_strict_json_without_nonfinite_literals(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self.run_cli(self.MISSING, "-n", "3", "--timeout", "10",
                                "--json", out)
            self.assertEqual(proc.returncode, 1, proc.stderr)
            # load_record itself rejects a bare NaN/Infinity via parse_constant.
            text, record = self.load_record(out)
            for literal in ("NaN", "Infinity", "-Infinity"):
                self.assertNotIn(literal, text)
            self.assertTrue(text.startswith("{"))
            self.assertTrue(text.endswith("}\n"), repr(text[-20:]))
            self.assertEqual([e["mode"] for e in record["modes"]], ["warm"])

    def test_all_failed_launches_report_null_stats_not_nan(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self.run_cli(self.MISSING, "-n", "2", "--timeout", "10",
                                "--json", out)
            self.assertEqual(proc.returncode, 1, proc.stderr)
            text, record = self.load_record(out)
            for entry in record["modes"]:
                results = entry["results"]
                self.assertEqual(results["n"], 0)
                self.assertIsNone(results["p50_ms"])
                self.assertIsNone(results["p95_ms"])
                self.assertIsNone(results["max_ms"])
                self.assertIsNone(results["max_rss"])
                self.assertEqual(results["successful"], 0)
            self.assertNotIn("NaN", text)
            self.assertIn("[warm] successful=0/2 p50=n/a p95=n/a max=n/a "
                          "invalid=[launch_error=2]", proc.stdout)

    def test_single_sample_percentiles_are_numbers_not_null(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self.run_cli(sys.executable, "-n", "1", "--timeout", "20",
                                "--json", out, "--", "-c", "pass")
            self.assertEqual(proc.returncode, 0, proc.stderr)
            _, record = self.load_record(out)
            results = record["modes"][0]["results"]
            elapsed = record["modes"][0]["samples"][0]["elapsed_ms"]
            self.assertEqual(results["n"], 1)
            self.assertEqual(results["p50_ms"], elapsed)
            self.assertEqual(results["p95_ms"], elapsed)
            self.assertEqual(results["max_ms"], elapsed)
            self.assertNotIn("p50=n/a", proc.stdout)

    def test_mixed_batch_prints_numeric_percentiles_over_successful_samples(self):
        # Exits 0 on odd launches and 4 on even ones, so one successful
        # sample feeds the percentiles while the other stays invalid evidence.
        script = (
            "import os, sys\n"
            "marker = sys.argv[1]\n"
            "if os.path.exists(marker):\n"
            "    os.unlink(marker)\n"
            "    raise SystemExit(0)\n"
            "open(marker, 'w').close()\n"
            "raise SystemExit(4)\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self.run_cli(sys.executable, "-n", "2", "--timeout", "20",
                                "--json", out, "--", "-c", script,
                                os.path.join(tmp, "alternate"))
            self.assertEqual(proc.returncode, 1, proc.stderr)
            text, record = self.load_record(out)
            summaries = [line for line in proc.stdout.splitlines()
                         if line.startswith("[warm] successful=")]
            self.assertEqual(len(summaries), 1, proc.stdout)
            self.assertIsNone(NONFINITE_WORD.search(summaries[0]), summaries[0])
            self.assertIn("successful=1/2", summaries[0])
            self.assertIn("invalid=[nonzero=1]", summaries[0])
            self.assertNotIn("n/a", summaries[0])
            self.assertNotIn("NaN", text)
            entry = record["modes"][0]
            results = entry["results"]
            self.assertEqual(results["n"], 1)
            self.assertEqual(results["invalid"], {"nonzero": 1})
            ok_sample = [s for s in entry["samples"] if s["outcome"] == startup.OK]
            self.assertEqual(results["p50_ms"], ok_sample[0]["elapsed_ms"])
            self.assertGreater(results["p50_ms"], 0.0)


class AllLaunchErrorsExitTests(CliContractTestCase):
    """A batch in which nothing launches cleanly must exit 1 and say why."""

    MISSING = "/nonexistent/nexus-perf-launch-error-target"

    def test_all_launch_errors_exit_1_with_stdout_and_record_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self.run_cli(self.MISSING, "-n", "3", "--timeout", "10",
                                "--json", out)
            self.assertEqual(proc.returncode, 1, proc.stderr)
            self.assertIn("[warm] successful=0/3 p50=n/a p95=n/a max=n/a "
                          "invalid=[launch_error=3]", proc.stdout)
            invalid_lines = [line for line in proc.stdout.splitlines()
                             if "invalid sample" in line]
            self.assertEqual(len(invalid_lines), 3, proc.stdout)
            for index, line in enumerate(invalid_lines):
                self.assertTrue(
                    line.startswith(f"[warm] invalid sample #{index}: "
                                    "outcome=launch_error exit_code=None "
                                    "signal=None "), line)
                self.assertRegex(line, ELAPSED_MS)
                self.assertIn(self.MISSING, line)
            _, record = self.load_record(out)
            entry = record["modes"][0]
            # A launch failure invalidates samples, not the measurement run.
            self.assertTrue(entry["valid"])
            self.assertIsNone(entry["invalid_reason"])
            self.assertEqual(entry["results"]["invalid"], {"launch_error": 3})
            self.assertEqual(entry["results"]["successful"], 0)
            self.assertEqual(entry["results"]["attempted"], 3)
            self.assertIsNone(entry["results"]["p50_ms"])
            self.assertEqual([s["outcome"] for s in entry["samples"]],
                             ["launch_error"] * 3)
            for sample in entry["samples"]:
                self.assertIsNone(sample["exit_code"])
                self.assertIsNone(sample["signal"])
                self.assertIn(self.MISSING, sample["error"])
                self.assertTrue(sample["error"].split(":")[0].endswith("Error"))

    def test_all_launch_errors_in_both_modes_exit_1(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self.run_cli(self.MISSING, "-n", "2", "--mode", "both",
                                "--timeout", "10", "--json", out)
            self.assertEqual(proc.returncode, 1, proc.stderr)
            self.assertIn("no --purge-cmd given", proc.stdout)
            _, record = self.load_record(out)
            self.assertEqual([e["mode"] for e in record["modes"]], ["warm", "cold"])
            for entry in record["modes"]:
                self.assertTrue(entry["valid"])
                self.assertEqual(entry["purge_runs"], 0)
                self.assertEqual(entry["results"]["invalid"], {"launch_error": 2})
                self.assertEqual(len(entry["samples"]), 2)
            self.assertIn("[cold] successful=0/2 p50=n/a p95=n/a max=n/a "
                          "invalid=[launch_error=2]", proc.stdout)

    @unittest.skipUnless(POSIX_SHELL, "purge command uses a POSIX shell")
    def test_purge_failure_exit_2_outranks_invalid_samples(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self.run_cli(self.MISSING, "-n", "1", "--mode", "both",
                                "--purge-cmd", "exit 9", "--timeout", "10",
                                "--json", out)
            # Cold invalidation (2) is reported even though warm also had
            # invalid samples (1).
            self.assertEqual(proc.returncode, 2, proc.stderr)
            _, record = self.load_record(out)
            warm, cold = record["modes"]
            # Warm keeps its own invalid-sample evidence and still summarizes.
            self.assertTrue(warm["valid"])
            self.assertEqual(warm["results"]["invalid"], {"launch_error": 1})
            self.assertEqual(warm["results"]["successful"], 0)
            # The cold purge failed before its first launch, so the cold mode
            # is invalid with purge evidence and no samples or stats at all.
            self.assertFalse(cold["valid"])
            self.assertIn("purge nonzero", cold["invalid_reason"])
            self.assertEqual(cold["purge_error"]["reason"], "nonzero")
            self.assertEqual(cold["purge_error"]["exit_code"], 9)
            self.assertIsNone(cold["purge_error"]["signal"])
            self.assertEqual(cold["purge_error"]["failed_at_sample"], 0)
            self.assertEqual(cold["purge_runs"], 1)
            self.assertEqual(cold["samples"], [])
            self.assertIsNone(cold["results"])

    def test_unwritable_json_path_exits_2_after_measuring(self):
        with tempfile.TemporaryDirectory() as tmp:
            for target in (tmp, os.path.join(tmp, "missing", "perf.json")):
                with self.subTest(target=target):
                    proc = self.run_cli(sys.executable, "-n", "1",
                                        "--timeout", "20", "--json", target,
                                        "--", "-c", "pass")
                    self.assertEqual(proc.returncode, 2, proc.stderr)
                    self.assertIn("could not write --json", proc.stderr)
                    # The batch still ran and reported; only the write failed.
                    self.assertIn("[warm] successful=1/1", proc.stdout)


class TimeoutAndRunsCliTests(CliContractTestCase):
    """Usable timeouts and run counts are enforced before any measurement."""

    def test_cli_rejects_zero_negative_and_nonfinite_timeouts(self):
        # The "=" form keeps argparse from treating a negative-looking token as
        # an option, so every value below reaches the timeout validator itself.
        for value in ("0", "0.0", "-0", "-1", "-1e-9", "nan", "NaN", "inf",
                      "-inf", "Infinity"):
            with self.subTest(timeout=value):
                with tempfile.TemporaryDirectory() as tmp:
                    out = os.path.join(tmp, "perf.json")
                    proc = self.run_cli(sys.executable, "-n", "1",
                                        f"--timeout={value}", "--json", out,
                                        "--", "-c", "pass")
                    self.assertEqual(proc.returncode, 2, proc.stderr)
                    self.assertIn("positive finite", proc.stderr)
                    # Rejected before launching: nothing measured or recorded.
                    self.assertEqual(proc.stdout, "", proc.stdout)
                    self.assertFalse(os.path.exists(out))

    def test_cli_rejects_unparsable_timeouts(self):
        for value in ("abc", "", "1,5", "0x10", "5s", "1 2"):
            with self.subTest(timeout=value):
                proc = self.run_cli(sys.executable, f"--timeout={value}",
                                    "--", "-c", "pass")
                self.assertEqual(proc.returncode, 2, proc.stderr)
                self.assertIn("invalid number", proc.stderr)
                self.assertEqual(proc.stdout, "", proc.stdout)

    def test_cli_rejects_runs_below_one_before_measuring(self):
        for value in ("0", "-1", "-20"):
            with self.subTest(runs=value):
                with tempfile.TemporaryDirectory() as tmp:
                    out = os.path.join(tmp, "perf.json")
                    proc = self.run_cli(sys.executable, "-n", value,
                                        "--timeout", "20", "--json", out,
                                        "--", "-c", "pass")
                    self.assertEqual(proc.returncode, 2, proc.stderr)
                    self.assertIn("--runs must be >= 1", proc.stderr)
                    self.assertEqual(proc.stdout, "", proc.stdout)
                    self.assertFalse(os.path.exists(out))

    def test_cli_rejects_non_integer_and_unknown_options(self):
        proc = self.run_cli(sys.executable, "-n", "abc", "--", "-c", "pass")
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertIn("invalid int value", proc.stderr)
        proc = self.run_cli(sys.executable, "--mode", "bogus", "--", "-c", "pass")
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertIn("invalid choice", proc.stderr)
        proc = self.run_cli(sys.executable, "--no-such-flag")
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertEqual(proc.stdout, "", proc.stdout)

    def test_usage_requires_a_binary_and_help_documents_the_contract(self):
        proc = self.run_cli()
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertIn("the following arguments are required: binary", proc.stderr)
        help_proc = self.run_cli("--help")
        self.assertEqual(help_proc.returncode, 0, help_proc.stderr)
        fragments = ("--runs", "--mode {warm,cold,both}", "--purge-cmd",
                     "--timeout", "positive and finite", "--build-profile",
                     "--json")
        for fragment in fragments:
            with self.subTest(fragment=fragment):
                self.assertIn(fragment, help_proc.stdout)


class ModeRecordTests(CliContractTestCase):
    """The warm/cold/both records, including unpurgeable cold attempts."""

    def test_warm_mode_record_has_no_purge_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self.run_cli(sys.executable, "-n", "2", "--mode", "warm",
                                "--purge-cmd", "exit 9", "--timeout", "20",
                                "--json", out, "--", "-c", "pass")
            # A configured but unrun purge must not affect warm mode.
            self.assertEqual(proc.returncode, 0, proc.stderr)
            _, record = self.load_record(out)
            entry = record["modes"][0]
            self.assertEqual(entry["purge_runs"], 0)
            self.assertIsNone(entry["purge_cmd"])
            self.assertEqual(entry["cache_prep"], "none")
            self.assertNotIn("purge_error", entry)
            self.assertEqual(entry["results"]["successful"], 2)
            self.assertEqual([s["outcome"] for s in entry["samples"]], ["ok"] * 2)
            self.assertEqual(record["methodology"]["cold_purge_cmd"], "exit 9")
            self.assertIn("configured for cold mode",
                          record["methodology"]["cold_cache_prep"])
            self.assertNotIn("cannot verify", proc.stdout)

    def test_cold_mode_without_purge_cmd_is_flagged_as_unprepared(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self.run_cli(sys.executable, "-n", "1", "--mode", "cold",
                                "--timeout", "20", "--json", out,
                                "--", "-c", "pass")
            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertIn("OS cache was NOT reset", proc.stdout)
            _, record = self.load_record(out)
            methodology = record["methodology"]
            self.assertEqual(methodology["mode"], "cold")
            self.assertEqual(methodology["cold_purge_cmd"], "none (cache not purged)")
            self.assertIn("unprepared", methodology["cold_cache_prep"])
            self.assertFalse(methodology["cache_reset_verified"])
            entry = record["modes"][0]
            self.assertEqual(entry["cache_prep"], "none")
            self.assertEqual(entry["purge_cmd"], None)
            self.assertEqual(entry["purge_runs"], 0)
            self.assertEqual(entry["results"]["successful"], 1)
            self.assertNotIn("cannot verify", proc.stdout)

    @unittest.skipUnless(POSIX_SHELL, "purge command uses a POSIX shell")
    def test_both_mode_records_warm_then_cold_with_purge_counts(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self.run_cli(sys.executable, "-n", "2", "--mode", "both",
                                "--purge-cmd", "true", "--timeout", "30",
                                "--json", out, "--", "-c", "pass")
            self.assertEqual(proc.returncode, 0, proc.stderr)
            _, record = self.load_record(out)
            self.assertEqual(record["methodology"]["mode"], "both")
            self.assertEqual(record["methodology"]["runs_per_mode"], 2)
            self.assertEqual([e["mode"] for e in record["modes"]], ["warm", "cold"])
            warm, cold = record["modes"]
            self.assertEqual(warm["purge_runs"], 0)
            self.assertIsNone(warm["purge_cmd"])
            self.assertEqual(cold["purge_runs"], 2)
            self.assertEqual(cold["purge_cmd"], "true")
            self.assertIn("cache reset not verified", cold["cache_prep"])
            for entry in record["modes"]:
                self.assertTrue(entry["valid"])
                self.assertEqual(set(entry), EXPECTED_MODE_FIELDS, sorted(entry))
                self.assertEqual(len(entry["samples"]), 2)
                self.assertEqual(entry["results"]["successful"], 2)
                self.assertEqual(entry["results"]["invalid"], {})
            self.assertIn("[warm] successful=2/2", proc.stdout)
            self.assertIn("[cold] successful=2/2", proc.stdout)
            self.assertIn("cannot verify", proc.stdout)

    @unittest.skipUnless(POSIX_SHELL, "purge command uses a POSIX shell")
    def test_both_mode_keeps_warm_results_and_partial_cold_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            marker = shlex.quote(os.path.join(tmp, "purge-once"))
            cmd = (f"if [ -e {marker} ]; then exit 5; "
                   f"else : > {marker}; fi")
            proc = self.run_cli(sys.executable, "-n", "3", "--mode", "both",
                                "--purge-cmd", cmd, "--timeout", "30",
                                "--json", out, "--", "-c", "pass")
            self.assertEqual(proc.returncode, 2, proc.stderr)
            self.assertIn("[cold] invalid: purge nonzero", proc.stdout)
            self.assertIn("(at sample 1, 1 collected)", proc.stdout)
            _, record = self.load_record(out)
            warm, cold = record["modes"]
            # Warm is unaffected by a cold purge failure.
            self.assertTrue(warm["valid"])
            self.assertEqual(warm["results"]["successful"], 3)
            self.assertEqual(len(warm["samples"]), 3)
            self.assertEqual(warm["purge_runs"], 0)
            # Cold keeps the attempts and the partial samples, and no stats.
            self.assertFalse(cold["valid"])
            self.assertEqual(cold["invalid_reason"],
                             "purge nonzero: purge command failed "
                             "(exit_code=5, signal=None)")
            self.assertEqual(set(cold), EXPECTED_MODE_FIELDS | {"purge_error"})
            self.assertEqual(cold["purge_runs"], 2)  # 1 succeeded, 1 failed
            self.assertEqual(cold["cache_prep"],
                             "purge failed; cache reset not verified")
            self.assertEqual(len(cold["samples"]), 1)
            self.assertEqual(cold["samples"][0]["outcome"], startup.OK)
            self.assertIsNone(cold["results"])
            self.assertEqual(cold["purge_error"]["reason"], "nonzero")
            self.assertEqual(cold["purge_error"]["exit_code"], 5)
            self.assertIsNone(cold["purge_error"]["signal"])
            self.assertEqual(cold["purge_error"]["failed_at_sample"], 1)
            self.assertGreater(cold["purge_error"]["elapsed_ms"], 0.0)

    def test_target_arguments_after_double_dash_are_not_harness_flags(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "perf.json")
            proc = self.run_cli(sys.executable, "-n", "1", "--mode", "warm",
                                "--timeout", "20", "--json", out,
                                "--", "-c", "pass", "--mode", "bogus", "-n", "99")
            self.assertEqual(proc.returncode, 0, proc.stderr)
            _, record = self.load_record(out)
            methodology = record["methodology"]
            self.assertEqual(methodology["mode"], "warm")
            self.assertEqual(methodology["runs_per_mode"], 1)
            self.assertEqual(methodology["launch_argv"],
                             [sys.executable, "-c", "pass", "--mode", "bogus",
                              "-n", "99"])
            self.assertNotIn("--", methodology["launch_argv"])
            entry = record["modes"][0]
            self.assertEqual(entry["results"]["successful"], 1)
            self.assertEqual(len(entry["samples"]), 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
