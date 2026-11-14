"""Recognize userspace markers despite interleaved timestamped kernel printk."""
import re


def userspace_bytes(console):
    return re.sub(rb"\[\s*\d+\.\d+\][^\n]*\n", b"", console)


if __name__ == '__main__':
    import unittest

    class ConsoleMarkers(unittest.TestCase):
        def test_split_freeze_marker(self):
            raw = b'CLONE FREEZ[    2.119968] sched_clock: unstable\nE seq=100\n'
            self.assertNotIn(b'CLONE FREEZE', raw)
            self.assertIn(b'CLONE FREEZE', userspace_bytes(raw))

        def test_failure_is_preserved(self):
            self.assertIn(b'CLONE FAIL clock', userspace_bytes(b'CLONE FAIL clock\n'))

    unittest.main()
