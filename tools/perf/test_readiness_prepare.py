import contextlib
import io
import json
import os
import signal
import sys
import tempfile
import time
import unittest
from unittest import mock

import readiness


@unittest.skipUnless(os.name == "posix", "preparation uses POSIX process groups")
class PreparationTests(unittest.TestCase):
    def test_per_sample_order_after_provenance_and_literal_arguments(self):
        with tempfile.TemporaryDirectory() as directory:
            log = os.path.join(directory, "events")
            script = "import sys; open(sys.argv[1], 'a').write(sys.argv[2] + '\\n')"
            prepare = (sys.executable, "-c", script, log, "prep;$(false)")
            cfg = readiness.RunConfig(prepare_argv=prepare, required=("ready",), quit_timeout=0.1)

            def provenance(*args):
                with open(log, "w") as handle:
                    handle.write("provenance\n")
                return {}

            def measure(config, command, index):
                return original(config, [sys.executable, "-c", script, log, "target"], index)

            original = readiness.run_measurement
            args = [sys.executable, "--runs", "2", "--require", "ready", "--ready-timeout", "0.1"]
            for arg in prepare:
                args.append("--prepare-arg=" + arg)
            with mock.patch.object(readiness, "collect_methodology", side_effect=provenance), \
                 mock.patch.object(readiness, "run_measurement", side_effect=measure), \
                 contextlib.redirect_stdout(io.StringIO()) as output, \
                 contextlib.redirect_stderr(io.StringIO()):
                readiness.main(args)
            record = json.loads(output.getvalue())
            with open(log) as handle:
                self.assertEqual(handle.read().splitlines(),
                                 ["provenance", "prep;$(false)", "target", "prep;$(false)", "target"])
            self.assertFalse(record["methodology"]["cache_reset_verified"])
            self.assertTrue(all(run["preparation"]["status"] == "ok" for run in record["runs"]))

    def test_failures_do_not_spawn_and_are_counted(self):
        for command, status in [(("/nonexistent/readiness-helper",), "error"),
                                ((sys.executable, "-c", "raise SystemExit(7)"), "nonzero"),
                                ((sys.executable, "-c", "import time; time.sleep(10)"), "timeout")]:
            with self.subTest(status=status), mock.patch.object(readiness, "spawn_pty") as spawn:
                cfg = readiness.RunConfig(prepare_argv=command, prepare_timeout=0.1)
                run = readiness.run_measurement(cfg, [sys.executable])
                self.assertEqual(run["outcome"], "preparation-error")
                self.assertEqual(run["preparation"]["status"], status)
                self.assertGreaterEqual(run["preparation"]["elapsed_ms"], 0)
                self.assertIn(run["outcome"], readiness.FAILURE_OUTCOMES)
                spawn.assert_not_called()

    def test_default_does_not_prepare(self):
        with mock.patch.object(readiness, "prepare_sample") as prepare, \
             mock.patch.object(readiness, "spawn_pty", side_effect=OSError("test")):
            run = readiness.run_measurement(readiness.RunConfig(), [sys.executable])
        prepare.assert_not_called()
        self.assertNotIn("preparation", run)

    def test_completed_helper_does_not_signal_a_reaped_process_group(self):
        with mock.patch.object(readiness.os, "killpg") as killpg:
            result = readiness.prepare_sample((sys.executable, "-c", "pass"), 2)
        self.assertEqual(result["status"], "ok")
        self.assertEqual(result["cleanup"], "leader-reaped; descendants-unverified")
        killpg.assert_not_called()

    def test_cli_failure_returns_nonzero_and_json(self):
        with mock.patch.object(readiness, "collect_methodology", return_value={}), \
             contextlib.redirect_stdout(io.StringIO()) as output, \
             contextlib.redirect_stderr(io.StringIO()):
            status = readiness.main([sys.executable, "--runs", "2",
                                     "--prepare-arg=/nonexistent/readiness-helper"])
        record = json.loads(output.getvalue())
        self.assertEqual(status, 1)
        self.assertEqual(record["summary"]["ready_ms"]["n"], 0)
        self.assertEqual([run["outcome"] for run in record["runs"]],
                         ["preparation-error", "preparation-error"])

    def test_timeout_requests_group_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            pidfile = os.path.join(directory, "pid")
            script = ("import subprocess,sys,time; "
                      "p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)']); "
                      "open(sys.argv[1],'w').write(str(p.pid)); time.sleep(30)")
            result = readiness.prepare_sample((sys.executable, "-c", script, pidfile), 0.5)
            self.assertEqual(result["status"], "timeout")
            self.assertEqual(result["cleanup"], "group-sigkill-requested")
            with open(pidfile) as handle:
                pid = int(handle.read())
            # A killed orphan can remain a zombie until its new parent reaps it.
            for _ in range(50):
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                if sys.platform.startswith("linux"):
                    try:
                        with open(f"/proc/{pid}/stat") as handle:
                            if handle.read().split(") ", 1)[1].startswith("Z "):
                                break
                    except FileNotFoundError:
                        break
                time.sleep(0.02)
            else:
                os.kill(pid, signal.SIGKILL)
                self.fail("helper descendant survived group cleanup")


if __name__ == "__main__":
    unittest.main()
