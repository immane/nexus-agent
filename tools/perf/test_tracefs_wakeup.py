"""Unprivileged tests: temporary files and mocks only; never access tracefs."""
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

import tracefs_wakeup as wakeup


class TracefsWakeupTests(unittest.TestCase):
    def test_ansi_filter_carries_state_across_reads(self):
        ansi = wakeup.AnsiFilter()
        self.assertEqual(ansi.feed(b'composer \x1b['), b'composer ')
        self.assertEqual(ansi.feed(b'31m(fixed)\x1b]x\x07'), b'(fixed)')

    def test_loss_counters_complete_and_changes_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cpu = root / 'per_cpu/cpu0'
            cpu.mkdir(parents=True)
            (cpu / 'stats').write_text('entries: 2\noverrun: 0\ncommit overrun: 0\ndropped events: 0\n')
            before = wakeup.stats(root)
            wakeup.check_loss(before, before)
            (cpu / 'stats').write_text('overrun: 1\ncommit overrun: 0\ndropped events: 0\n')
            with self.assertRaisesRegex(RuntimeError, 'overrun changed'):
                wakeup.check_loss(before, wakeup.stats(root))
            (cpu / 'stats').write_text('overrun: 0\n')
            with self.assertRaisesRegex(RuntimeError, 'incomplete'):
                wakeup.stats(root)
            with self.assertRaisesRegex(RuntimeError, 'set changed'):
                wakeup.check_loss(before, {})

    def test_identity_fails_closed(self):
        account = SimpleNamespace(pw_uid=123, pw_gid=456)
        status = 'Uid:\t123 123 123 123\nGid:\t456 456 456 456\nGroups:\nCapEff:\t00000000\nNoNewPrivs:\t1\n'
        with mock.patch.object(Path, 'read_text', return_value=status):
            wakeup.verify_identity(42, account)
        for bad in (status.replace('123 123 123 123', '0 123 123 123'),
                    status.replace('Groups:\n', 'Groups:\t1\n'),
                    status.replace('NoNewPrivs:\t1', 'NoNewPrivs:\t0'),
                    status.replace('CapEff:\t00000000', 'CapEff:\t00000001')):
            with mock.patch.object(Path, 'read_text', return_value=bad), self.assertRaises(RuntimeError):
                wakeup.verify_identity(42, account)

    def test_spawn_failure_cleans_own_home_and_disables_only_instance(self):
        account = SimpleNamespace(pw_uid=123, pw_gid=456, pw_name='test')
        with tempfile.TemporaryDirectory() as directory:
            instance = Path(directory) / 'instance'
            instance.mkdir()
            (instance / 'tracing_on').write_text('1')
            with mock.patch.object(wakeup.os, 'chown'), mock.patch.object(wakeup.subprocess, 'Popen', side_effect=OSError('spawn failed')):
                result = wakeup.sample(instance, '/not-a-real-executable', account, 0.01, 0.01)
            self.assertEqual(result['status'], 'failed')
            self.assertIn('spawn failed', result['error'])
            self.assertTrue(all(result['cleanup'].values()))
            self.assertEqual((instance / 'tracing_on').read_text(), '0')

    def test_non_tracefs_mount_rejected(self):
        with mock.patch.object(Path, 'read_text', return_value='1 2 0:1 / /sys/kernel/tracing rw - tmpfs tmpfs rw'):
            with self.assertRaises(RuntimeError):
                wakeup.verify_tracefs()

    def test_reaped_target_does_not_signal_reused_process_group(self):
        process = mock.Mock(pid=12345)
        process.poll.return_value = 0
        with mock.patch.object(wakeup.os, 'killpg') as killpg:
            wakeup.stop_target(process)
        killpg.assert_not_called()


if __name__ == '__main__':
    unittest.main()
