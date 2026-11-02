"""Tests for the ECHO-sampled composer probe, using local PTY children."""

from unittest import mock

import readiness
from test_readiness import PtyTestCase


TOKEN = "0123456789abcdef"


class ComposerProbeTests(PtyTestCase):
    def run_probe(self, body, **options):
        script = self.script("composer_probe.py", body)
        with mock.patch.object(readiness.secrets, "token_hex", return_value=TOKEN):
            return self.run_target(script, probe_composer=True, probe_timeout=0.3,
                                   quit_timeout=0.1, kill_grace=0.2, **options)["input_probe"]

    def test_raw_target_incrementally_paints_token(self):
        probe = self.run_probe(r'''
import sys, tty
tty.setraw(0)
sys.stdout.write(FRAME); sys.stdout.flush()
sys.stdout.write("\x1b[10;1H"); sys.stdout.flush()
while True:
    b = sys.stdin.buffer.read(1)
    if not b or b == b"\x04": break
    sys.stdout.buffer.write(b); sys.stdout.buffer.flush()
''')
        self.assertEqual(probe["status"], "observed-composer-response")
        self.assertFalse(probe["echo_before"])
        self.assertFalse(probe["echo_after"])
        self.assertGreaterEqual(probe["ms"], 0)
        self.assertGreaterEqual(probe["spawn_to_response_ms"], probe["ms"])

    def test_enabled_driver_echo_refuses_to_send(self):
        probe = self.run_probe(r'''
import sys, time
sys.stdout.write(FRAME); sys.stdout.flush()
time.sleep(10)
''')
        self.assertEqual(probe["status"], "echo-enabled")
        self.assertFalse(probe["wrote_keys"])
        self.assertIsNone(probe["spawn_to_response_ms"])

    def test_preexisting_marker_rejected(self):
        probe = self.run_probe(r'''
import sys, tty
tty.setraw(0)
sys.stdout.write(FRAME + "0123456789abcdef"); sys.stdout.flush()
while sys.stdin.buffer.read(1) != b"\x04": pass
''')
        self.assertEqual(probe["status"], "marker-preexisting")
        self.assertFalse(probe["wrote_keys"])

    def test_cross_row_output_cannot_match(self):
        probe = self.run_probe(r'''
import sys, tty
tty.setraw(0)
sys.stdout.write(FRAME); sys.stdout.flush()
data = sys.stdin.buffer.read(16)
sys.stdout.buffer.write(b"\x1b[10;1H" + data[:8] + b"\x1b[11;1H" + data[8:]); sys.stdout.buffer.flush()
while sys.stdin.buffer.read(1) != b"\x04": pass
''')
        self.assertEqual(probe["status"], "marker-not-observed")
        self.assertIsNone(probe["ms"])

    def test_echo_unavailable_does_not_send(self):
        with mock.patch.object(readiness.PtyTarget, "echo_enabled", return_value=None):
            probe = self.run_probe(r'''
import sys, tty
tty.setraw(0)
sys.stdout.write(FRAME); sys.stdout.flush()
while sys.stdin.buffer.read(1) != b"\x04": pass
''')
        self.assertEqual(probe["status"], "echo-unavailable")
        self.assertFalse(probe["wrote_keys"])

    def test_echo_changed_after_write_invalidates_observation(self):
        with mock.patch.object(readiness.PtyTarget, "echo_enabled", side_effect=[False, True]):
            probe = self.run_probe(r'''
import sys, tty
tty.setraw(0)
sys.stdout.write(FRAME); sys.stdout.flush()
sys.stdout.buffer.write(sys.stdin.buffer.read(16)); sys.stdout.buffer.flush()
while sys.stdin.buffer.read(1) != b"\x04": pass
''')
        self.assertEqual(probe["status"], "echo-unconfirmed")
        self.assertIsNone(probe["spawn_to_response_ms"])
