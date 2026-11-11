"""Keep container shutdown defaults outside the admitted RPC drain cap."""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]


class ShutdownOrderingTest(unittest.TestCase):
    def test_default_timeout_ordering(self):
        compose = (ROOT / "compose.yaml").read_text()
        entrypoint = (ROOT / "docker/entrypoint.sh").read_text()
        drain = int(re.search(r'TERRARIUM_UPDATE_DRAIN_WAIT:-([0-9]+)', compose)[1])
        daemon = int(re.search(r'TERRARIUM_DAEMON_STOP_TIMEOUT:-([0-9]+)', entrypoint)[1])
        runners = int(re.search(r'TERRARIUM_SHUTDOWN_GRACE:-([0-9]+)', entrypoint)[1])
        container = int(re.search(r'stop_grace_period: ([0-9]+)s', compose)[1])
        fallback = int(re.search(r'DEFAULT_STOP_TIMEOUT: u64 = ([0-9]+)',
                                (ROOT / "crates/terrarium-updater/src/docker.rs").read_text())[1])
        self.assertLess(drain, daemon)
        self.assertLess(daemon + runners, container)
        self.assertEqual(container, fallback)
        self.assertLess(container, 15 * 60)
