"""Only an explicit valid proxy network may influence the nginx real-IP module."""
from pathlib import Path
import subprocess
import sys
import unittest

SCRIPT = Path(__file__).with_name("write-trusted-edge.py")


class EdgeTests(unittest.TestCase):
    def test_exact_trusted_proxy_network_is_used(self):
        result = subprocess.run([sys.executable, str(SCRIPT), "172.18.0.1/32"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("set_real_ip_from 172.18.0.1/32;", result.stdout)
        self.assertIn("real_ip_header X-Forwarded-For;", result.stdout)

    def test_wildcard_trust_and_directive_injection_are_rejected(self):
        for value in ("0.0.0.0/0", "::/0", "127.0.0.1; include /evil;", "999.1.1.1/32"):
            with self.subTest(value=value):
                result = subprocess.run([sys.executable, str(SCRIPT), value], capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("set_real_ip_from", result.stdout)


if __name__ == "__main__":
    unittest.main()
