"""Exercise the deploy controller with command-boundary doubles (no staging writes)."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

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
        for path in binaries.iterdir():
            path.chmod(0o755)
        self.env = dict(os.environ, PATH=str(binaries) + os.pathsep + os.environ["PATH"],
                        COMMAND_LOG=str(self.log), API_IMAGE="example/api:sha-new", WORKER_IMAGE="example/api:sha-new")

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
        result = self.run_deploy(API_IMAGE="example/api:sha-old", WORKER_IMAGE="example/api:sha-old")
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

if __name__ == "__main__":
    unittest.main()
