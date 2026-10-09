#!/usr/bin/env python3
"""Behavioral tests of deprecation registry validation and changelog generation."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


class DeprecationPolicyTests(unittest.TestCase):
    def fixture(self, sunset="2027-04-09T00:00:00Z"):
        return {"entries": [{"element": "legacyCatalog", "method": "GET", "path": "/api/v1/catalog/{id}", "deprecated_at": "2026-10-09T00:00:00Z", "sunset_at": sunset, "replacement": "GET /api/v1/catalog/items/{id}"}]}

    def run_policy(self, registry, *args, prior=None):
        with tempfile.TemporaryDirectory() as directory:
            source, output = Path(directory) / "registry.json", Path(directory) / "changes.md"
            source.write_text(json.dumps(registry), encoding="utf-8")
            if prior is not None:
                output.write_text(prior, encoding="utf-8")
            result = subprocess.run([sys.executable, str(Path(__file__).with_name("deprecation-policy.py")), "--registry", str(source), "--output", str(output), *args], text=True, capture_output=True)
            return result, output.read_text(encoding="utf-8") if output.exists() else None

    def test_valid_deprecation_generates_a_release_entry(self):
        result, output = self.run_policy(self.fixture(), "--write")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("legacyCatalog", output)
        self.assertIn("2027-04-09", output)
        self.assertIn("GET /api/v1/catalog/items/{id}", output)
        result, _ = self.run_policy(self.fixture(), "--check", prior=output)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_short_sunset_fails_without_writing_a_changelog(self):
        result, output = self.run_policy(self.fixture("2027-04-08T23:59:59Z"), "--write")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("six calendar months", result.stderr)
        self.assertIsNone(output)

    def test_stale_changelog_fails_the_build_check(self):
        result, _ = self.run_policy(self.fixture(), "--check", prior="# Old\n")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("stale", result.stderr)

    def test_month_end_window_uses_calendar_months(self):
        fixture = self.fixture("2027-02-28T00:00:00Z")
        fixture["entries"][0]["deprecated_at"] = "2026-08-31T00:00:00Z"
        result, _ = self.run_policy(fixture, "--write")
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
