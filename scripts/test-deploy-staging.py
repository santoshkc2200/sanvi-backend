"""Exercise the deploy controller with command-boundary doubles (no staging writes)."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import sys

ROOT = Path(__file__).resolve().parent.parent
BASH = "C:/Program Files/Git/bin/bash.exe" if os.name == "nt" else "bash"

class DeployTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "scripts").mkdir()
        self.script = self.root / "scripts/deploy-staging.sh"
        shutil.copyfile(ROOT / "scripts/deploy-staging.sh", self.script)
        shutil.copyfile(ROOT / "scripts/check-deployment-migrations.py", self.root / "scripts/check-deployment-migrations.py")
        self.runtime = self.root / ".deploy/proxy"
        self.runtime.mkdir(parents=True)
        self.active = self.runtime / "active.conf"
        self.active.write_text("# active: api\nupstream sanvi_api { server api:8080; }\n")
        self.log = self.root / "commands"
        binaries = self.root / "bin"
        binaries.mkdir()
        docker = binaries / "docker"
        docker.write_text('''#!/usr/bin/env bash
echo "$*" >> "$COMMAND_LOG"
case "$*" in
  "create "*) echo fixture-image ;;
  "cp "*)
    if [[ "$2" == *migrations/. ]]; then cp "$MIGRATION_FIXTURE/2_rewrite.sql" "$3/";
    elif [[ "$2" == *image-contract ]]; then
      [[ "${LEGACY_IMAGE:-0}" != 1 ]] || exit 1
      echo sanvi-hardened-v1 > "$3"
    else
      [[ "${MISSING_POLICY:-0}" != 1 ]] || exit 1
      cp "$MIGRATION_FIXTURE/policy.json" "$3"
    fi ;;
  *"migrate status"*) echo "no migrations applied" ;;
  *"migrate up"*) [[ "${FAIL_MIGRATE:-0}" != 1 ]] ;;
  *"nginx -t"*) [[ "${FAIL_VALIDATE:-0}" != 1 ]] ;;
  *"up -d"*) [[ "${FAIL_START:-0}" != 1 ]] ;;
  *"ps -o args"*) [[ "${DRAINING:-0}" != 1 ]] || echo "nginx: worker process is shutting down" ;;
  *"ps -o pid,args"*)
    count=$(grep -c 'nginx -s reload' "$COMMAND_LOG" || true)
    echo "$((500 + count)) nginx: worker process" ;;
esac
''', newline="\n")
        # flock is supplied by util-linux on the staging host and Linux CI.
        # Git Bash lacks it; lock contention is tested through its exit status.
        (binaries / "flock").write_text('#!/usr/bin/env bash\n[[ "${LOCKED:-0}" != 1 ]]\n', newline="\n")
        (binaries / "sleep").write_text('#!/usr/bin/env bash\nexit 0\n', newline="\n")
        (binaries / "python3").write_text('''#!/usr/bin/env bash
args=()
for arg in "$@"; do
  if [[ "$(uname -s)" == MINGW* && -e "$arg" ]]; then
    args+=("$(cygpath -m "$arg")")
  else args+=("$arg"); fi
done
exec "$TEST_PYTHON" "${args[@]}"
''', newline="\n")
        for path in binaries.iterdir():
            path.chmod(0o755)
        self.env = dict(os.environ, PATH=str(binaries) + os.pathsep + os.environ["PATH"],
                        COMMAND_LOG=str(self.log), API_IMAGE="example/api@sha256:" + "a" * 64,
                        WORKER_IMAGE="example/api@sha256:" + "a" * 64)
        self.secret = self.root / "runtime.env"
        self.secret.write_text("SANVI__DATABASE__PASSWORD=private-test-value\n")
        self.secret.chmod(0o600)
        self.env["SANVI_RUNTIME_ENV_FILE"] = str(self.secret)
        self.env["DEPLOY_DRAIN_TIMEOUT_SECS"] = "2"
        self.env["TEST_PYTHON"] = sys.executable.replace("\\", "/")
        fixture = self.root / "migrations"
        fixture.mkdir()
        (fixture / "2_rewrite.sql").write_text("ALTER TABLE t RENAME TO old_t;")
        (fixture / "policy.json").write_text('{"2_rewrite.sql":"rolling"}')
        self.env["MIGRATION_FIXTURE"] = str(fixture).replace("\\", "/")
        self.env["SANVI_MIGRATOR_ENV_FILE"] = str(self.secret)

    def run_deploy(self, **overrides):
        return subprocess.run([BASH, str(self.script)], env=dict(self.env, **overrides), capture_output=True, text=True)

    def test_replacement_is_healthy_before_reload_and_active_slot_is_never_recreated(self):
        result = self.run_deploy()
        self.assertEqual(result.returncode, 0, result.stderr)
        commands = self.log.read_text().splitlines()
        start = next(i for i, cmd in enumerate(commands) if "up -d" in cmd)
        validate = next(i for i, cmd in enumerate(commands) if "nginx -t" in cmd)
        reload = next(i for i, cmd in enumerate(commands) if "nginx -s reload" in cmd)
        self.assertLess(start, validate)
        self.assertLess(validate, reload)
        self.assertIn("--wait api_next worker", commands[start])
        self.assertTrue(self.active.read_text().startswith("# active: api_next\n"))
        self.assertFalse(any("stop " in cmd or " down" in cmd for cmd in commands))
        # Revert uses the other slot, through the identical controller.
        result = self.run_deploy(API_IMAGE="example/api@sha256:" + "b" * 64,
                                 WORKER_IMAGE="example/api@sha256:" + "b" * 64)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(self.active.read_text().startswith("# active: api\n"))
        self.assertIn("--wait api worker", self.log.read_text())

    def test_failed_candidate_or_config_preserves_active_backend(self):
        previous = self.active.read_text()
        for flag in ("FAIL_START", "FAIL_VALIDATE"):
            with self.subTest(flag=flag):
                result = self.run_deploy(**{flag: "1"})
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.active.read_text(), previous)
                self.assertNotIn("nginx -s reload", self.log.read_text())

    def test_interrupted_config_replacement_is_reconciled_before_slot_reuse(self):
        # Durable state from a killed controller after mv but before reload:
        # the new candidate is healthy, but nginx still serves the old slot.
        self.active.write_text("# active: api_next\nupstream sanvi_api { server api_next:8080; }\n")
        pending = self.root / ".deploy/switch.pending"
        pending.touch()
        result = self.run_deploy()
        self.assertEqual(result.returncode, 0, result.stderr)
        commands = self.log.read_text().splitlines()
        recovered = next(i for i, cmd in enumerate(commands) if "nginx -s reload" in cmd)
        reused = next(i for i, cmd in enumerate(commands) if "up -d" in cmd)
        self.assertLess(recovered, reused, "serving state must be reconciled before recreating api")
        self.assertIn("--wait api worker", commands[reused])
        self.assertFalse(pending.exists())

    def test_undrained_requests_and_concurrent_deploy_block_slot_reuse(self):
        previous = self.active.read_text()
        for flag in ("DRAINING", "LOCKED"):
            with self.subTest(flag=flag):
                result = self.run_deploy(**{flag: "1"})
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.active.read_text(), previous)
                self.assertNotIn("up -d", self.log.read_text() if self.log.exists() else "")

    def test_missing_runtime_secret_file_fails_before_touching_containers(self):
        result = self.run_deploy(SANVI_RUNTIME_ENV_FILE=str(self.root / "missing.env"))
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.log.exists(), "containers must not be changed without runtime configuration")

    def test_mutable_tag_is_rejected_before_touching_containers(self):
        result = self.run_deploy(API_IMAGE="example/api:main")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.log.exists())

    def test_api_and_worker_must_share_the_validated_image_contract(self):
        result = self.run_deploy(WORKER_IMAGE="example/api@sha256:" + "b" * 64)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("same image", result.stderr)
        self.assertFalse(self.log.exists())

    def test_proxy_replaces_untrusted_forwarding_headers(self):
        result = self.run_deploy()
        self.assertEqual(result.returncode, 0, result.stderr)
        config = self.active.read_text()
        self.assertIn("proxy_set_header X-Forwarded-For $remote_addr;", config)
        self.assertIn('proxy_set_header Forwarded "";', config)
        self.assertIn('proxy_set_header X-Forwarded-Host "";', config)
        self.assertIn('proxy_set_header X-Real-IP "";', config)
        self.assertNotIn("$proxy_add_x_forwarded_for", config)

    def test_rolling_deploy_rejects_pending_maintenance_before_mutating_services(self):
        (self.root / "migrations/policy.json").write_text('{"2_rewrite.sql":"maintenance"}')
        result = self.run_deploy()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("maintenance mode required", result.stderr)
        commands = self.log.read_text()
        self.assertNotIn("migrate up", commands)
        self.assertNotIn(" stop ", commands)
        self.assertNotIn("up -d", commands)

    def test_maintenance_stops_all_old_binaries_before_migration_then_starts_new_ones(self):
        result = self.run_deploy(DEPLOYMENT_MODE="maintenance")
        self.assertEqual(result.returncode, 0, result.stderr)
        commands = self.log.read_text().splitlines()
        stop = next(i for i, command in enumerate(commands) if "stop api_proxy api api_next worker" in command)
        migrate = next(i for i, command in enumerate(commands) if "migrate up" in command)
        start = next(i for i, command in enumerate(commands) if "up -d" in command)
        self.assertLess(stop, migrate)
        self.assertLess(migrate, start)
        self.assertFalse((self.root / ".deploy/maintenance.pending").exists())

    def test_failed_maintenance_keeps_services_stopped_and_requires_explicit_recovery(self):
        result = self.run_deploy(DEPLOYMENT_MODE="maintenance", FAIL_MIGRATE="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue((self.root / ".deploy/maintenance.pending").exists())
        self.assertNotIn("up -d", self.log.read_text())
        result = self.run_deploy()
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("up -d", self.log.read_text())

    def test_legacy_image_missing_policy_is_rejected_without_changing_serving_state(self):
        original = self.active.read_text()
        trusted = self.runtime / "trusted-realip.conf"
        trusted.write_text("# original trust boundary\n")
        for flag in ("MISSING_POLICY", "LEGACY_IMAGE"):
            with self.subTest(flag=flag):
                result = self.run_deploy(**{flag: "1"})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("unsupported image", result.stderr)
                self.assertIn("sanvi-hardened-v1", result.stderr)
                self.assertEqual(self.active.read_text(), original)
                self.assertEqual(trusted.read_text(), "# original trust boundary\n")
                commands = self.log.read_text()
                self.assertNotIn("migrate up", commands)
                self.assertNotIn("stop api_proxy", commands)
                self.assertNotIn("up -d", commands)

if __name__ == "__main__":
    unittest.main()
