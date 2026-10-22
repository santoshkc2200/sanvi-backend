"""A pending maintenance migration must never enter an ordinary rolling deploy."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("check-deployment-migrations.py")


class MigrationPolicyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.migrations = self.root / "migrations"
        self.migrations.mkdir()
        (self.migrations / "1_expand.sql").write_text("ALTER TABLE t ADD COLUMN note text;")
        (self.migrations / "2_rewrite.sql").write_text("ALTER TABLE t RENAME TO old_t;")
        self.policy = self.root / "policy.json"
        self.policy.write_text(json.dumps({"1_expand.sql": "rolling", "2_rewrite.sql": "maintenance"}))
        self.applied = self.root / "applied.txt"
        self.applied.write_text("1 expand\n")

    def run_policy(self, mode="rolling"):
        return subprocess.run([sys.executable, str(SCRIPT), "--migrations-dir", str(self.migrations),
                               "--policy", str(self.policy), "--applied-file", str(self.applied),
                               "--mode", mode], text=True, capture_output=True)

    def test_pending_rewrite_is_rejected_in_rolling_mode(self):
        result = self.run_policy()
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("2_rewrite.sql", result.stderr)

    def test_applied_maintenance_migration_does_not_block_later_rolling_deploy(self):
        self.applied.write_text("1 expand\n2 rewrite\n")
        self.assertEqual(self.run_policy().returncode, 0)

    def test_explicit_maintenance_mode_allows_the_pending_rewrite(self):
        self.assertEqual(self.run_policy("maintenance").returncode, 0)

    def test_new_unclassified_migration_fails_closed(self):
        (self.migrations / "3_new.sql").write_text("SELECT 1;")
        result = self.run_policy("maintenance")
        self.assertEqual(result.returncode, 1)
        self.assertIn("unclassified", result.stderr)

    def test_broken_status_output_cannot_be_treated_as_an_empty_database(self):
        self.applied.write_text("database query failed\n")
        result = self.run_policy("maintenance")
        self.assertEqual(result.returncode, 1)
        self.assertIn("status", result.stderr)

    def test_checked_in_migrations_are_explicitly_classified(self):
        root = SCRIPT.parent.parent
        self.applied.write_text("no migrations applied\n")
        result = subprocess.run([sys.executable, str(SCRIPT), "--migrations-dir", str(root / "migrations"),
                                 "--policy", str(root / "deployment/migration-policy.json"),
                                 "--applied-file", str(self.applied), "--mode", "maintenance"],
                                text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_candidate_older_than_applied_schema_is_rejected_even_in_maintenance_mode(self):
        self.applied.write_text("1 expand\n2 rewrite\n3 newer_schema\n")
        result = self.run_policy("maintenance")
        self.assertEqual(result.returncode, 1)
        self.assertIn("unsupported schema", result.stderr)
        self.assertIn("3", result.stderr)


if __name__ == "__main__":
    unittest.main()
