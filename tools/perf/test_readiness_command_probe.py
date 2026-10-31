"""PTY command-panel probe: response, terminal echo, and stale output."""

import unittest

from test_readiness import PtyTestCase, BODY_FRAME_WAIT_QUIT
import readiness


class ProbeScreenTests(unittest.TestCase):
    def test_unrelated_rows_do_not_form_a_marker(self):
        screen = readiness.ProbeScreen(80, 24)
        screen.feed(b"\x1b[2;1HEnter/Tab \x1b[3;1Hinsert")
        self.assertFalse(screen.contains(readiness.COMMAND_PROBE_MARKER))

    def test_split_cursor_sequence_and_erasure(self):
        screen = readiness.ProbeScreen(80, 24)
        screen.feed(b"\x1b[2;1HEnter/Tab Xnsert\x1b[2;")
        screen.feed(b"11Hi")
        self.assertTrue(screen.contains(readiness.COMMAND_PROBE_MARKER))
        screen.feed(b"\x1b[2;1H\x1b[2K")
        self.assertFalse(screen.contains(readiness.COMMAND_PROBE_MARKER))


class CommandProbeTests(PtyTestCase):
    def test_response_is_not_input_echo(self):
        script = self.script("command.py", r'''
import sys, tty
tty.setraw(0)
sys.stdout.write(FRAME); sys.stdout.flush()
while True:
    b = sys.stdin.buffer.read(1)
    if b == b"\x04" or not b: break
    if b == b":":
        sys.stdout.write("Enter/Tab "); sys.stdout.flush()
        sys.stdout.write("insert"); sys.stdout.flush()
''')
        result = self.run_target(script, probe_command=True)
        probe = result["input_probe"]
        self.assertEqual(probe["status"], "observed-command-panel")
        self.assertGreaterEqual(probe["ms"], 0)
        self.assertGreaterEqual(probe["spawn_to_response_ms"], result["ready"]["ms"])

    def test_driver_echo_without_reading_is_not_response(self):
        script = self.script("driver.py", r'''
import sys, time
sys.stdout.write(FRAME); sys.stdout.flush()
time.sleep(10)
''')
        result = self.run_target(script, probe_command=True, probe_timeout=0.6,
                                 quit_timeout=0.1, kill_grace=0.2)
        self.assertEqual(result["input_probe"]["status"], "marker-not-observed")
        self.assertIsNone(result["input_probe"]["spawn_to_response_ms"])

    def test_preexisting_marker_does_not_count(self):
        script = self.script("preexisting.py", BODY_FRAME_WAIT_QUIT.replace(
            'sys.stdout.write(FRAME)', 'sys.stdout.write(FRAME + "Enter/Tab insert")'))
        probe = self.run_target(script, probe_command=True)["input_probe"]
        self.assertEqual(probe["status"], "marker-preexisting")
        self.assertFalse(probe["wrote_keys"])
        self.assertIsNone(probe["ms"])

    def test_escape_precedes_colon_and_incremental_screen_is_reconstructed(self):
        script = self.script("incremental.py", r'''
import sys, tty
tty.setraw(0)
sys.stdout.write(FRAME + "\x1b[10;1HEnter/Tab Xnsert"); sys.stdout.flush()
viewport = False
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b == b"\x04": break
    if b == b"\x1b": viewport = True
    elif b == b":" and viewport:
        sys.stdout.write("\x1b[10;11Hi"); sys.stdout.flush()
''')
        probe = self.run_target(script, probe_command=True)["input_probe"]
        self.assertEqual(probe["status"], "observed-command-panel")
        self.assertTrue(probe["wrote_escape"])
        self.assertTrue(probe["wrote_keys"])

    def test_marker_during_escape_settle_does_not_count(self):
        script = self.script("escape_marker.py", r'''
import sys, tty
tty.setraw(0)
sys.stdout.write(FRAME); sys.stdout.flush()
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b == b"\x04": break
    if b == b"\x1b":
        sys.stdout.write("Enter/Tab insert"); sys.stdout.flush()
''')
        probe = self.run_target(script, probe_command=True)["input_probe"]
        self.assertEqual(probe["status"], "marker-preexisting")
        self.assertFalse(probe["wrote_keys"])


if __name__ == "__main__":
    unittest.main()
