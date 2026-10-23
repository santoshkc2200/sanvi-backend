"""Validate actual Compose resolution without starting any deployment services."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import uuid

ROOT = Path(__file__).resolve().parent.parent
DOCKER = shutil.which("docker") or "C:/Program Files/Docker/Docker/resources/bin/docker.exe"


class ComposeTests(unittest.TestCase):
    def test_runtime_secret_values_are_injected_literally_and_worker_probes_its_own_listener(self):
        with tempfile.TemporaryDirectory() as temporary:
            secret = Path(temporary) / "runtime.env"
            sentinel = 'sentinel-$literal-"quoted"'
            secret.write_text("SANVI__DATABASE__PASSWORD=" + sentinel + "\n")
            env = dict(os.environ, API_IMAGE="example/api@sha256:" + "a" * 64,
                       WORKER_IMAGE="example/api@sha256:" + "a" * 64,
                       SANVI_RUNTIME_ENV_FILE=str(secret), DOCKER_API_VERSION="1.47")
            result = subprocess.run([DOCKER, "compose", "-f", str(ROOT / "docker-compose.staging.yml"),
                                     "config", "--format", "json"], env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            services = json.loads(result.stdout)["services"]
            for service in ("api", "api_next", "worker"):
                self.assertIn("SANVI__DATABASE__PASSWORD", services[service]["environment"])
            probe = services["worker"]["healthcheck"]["test"]
            self.assertEqual(probe[probe.index("--port") + 1], "9091")
            self.assertEqual(probe[probe.index("--path") + 1], "/readyz")
            self.assertFalse(services["worker"].get("ports"), "management must remain private")
            self.assertEqual(services["api"]["image"], env["API_IMAGE"])
            proxy_ip = services["api_proxy"]["networks"]["default"]["ipv4_address"]
            for service in ("api", "api_next"):
                self.assertEqual(json.loads(services[service]["environment"]["SANVI__SERVER__TRUSTED_PROXY_CIDRS"]),
                                 [proxy_ip + "/32"])
            # Compose serializes literal dollars as $$ in canonical output.
            # Inspect a CREATED (never started) fixture container to prove the
            # value that actually reaches Docker, rather than guessing escaping.
            env["WORKER_IMAGE"] = os.environ.get("SANVI_DEPLOY_TEST_IMAGE", "alpine:3.19")
            project = "sanvi-config-test-" + uuid.uuid4().hex[:10]
            compose = [DOCKER, "compose", "-p", project, "-f", str(ROOT / "docker-compose.staging.yml")]
            try:
                created = subprocess.run(compose + ["create", "--no-build", "--pull", "never", "worker"],
                                         env=env, capture_output=True, text=True)
                self.assertEqual(created.returncode, 0, created.stderr)
                container = subprocess.run(compose + ["ps", "--all", "-q", "worker"], env=env,
                                           capture_output=True, text=True, check=True).stdout.strip()
                inspected = subprocess.run([DOCKER, "inspect", container], env=env,
                                           capture_output=True, text=True, check=True)
                actual_env = json.loads(inspected.stdout)[0]["Config"]["Env"]
                self.assertIn("SANVI__DATABASE__PASSWORD=" + sentinel, actual_env)
            finally:
                subprocess.run(compose + ["down"], env=env, capture_output=True, check=True)

    def test_missing_runtime_file_is_a_configuration_error(self):
        env = dict(os.environ, API_IMAGE="example/api@sha256:" + "a" * 64,
                   WORKER_IMAGE="example/api@sha256:" + "a" * 64,
                   SANVI_RUNTIME_ENV_FILE=str(ROOT / "nonexistent-deployment-secrets.env"))
        result = subprocess.run([DOCKER, "compose", "-f", str(ROOT / "docker-compose.staging.yml"),
                                 "config", "--format", "json"], env=env, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
