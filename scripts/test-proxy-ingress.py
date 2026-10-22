"""Send forged headers through the generated nginx configuration to a real backend."""
import importlib.util
import ipaddress
import json
import os
from pathlib import Path
import subprocess
import time
import unittest
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("controller_tests", ROOT / "scripts/test-deploy-staging.py")
controller_tests = importlib.util.module_from_spec(spec)
spec.loader.exec_module(controller_tests)
DOCKER = "C:/Program Files/Docker/Docker/resources/bin/docker.exe" if os.name == "nt" else "docker"


class ProxyTests(unittest.TestCase):
    def test_client_forwarding_claims_are_discarded_at_the_actual_ingress(self):
        controller = controller_tests.DeployTests()
        controller.setUp()
        self.addCleanup(controller.doCleanups)
        deployed = controller.run_deploy()
        self.assertEqual(deployed.returncode, 0, deployed.stderr)
        active = controller.active.read_text().replace("server api_next:8080;", "server 127.0.0.1:8081;")
        # Test-only observation of nginx's resolved peer identity.
        active = active.replace("proxy_http_version 1.1;",
                                "proxy_http_version 1.1;\n        proxy_set_header X-Test-Observed-Peer $remote_addr;")
        active += '''\nserver {
    listen 8081;
    location / {
        default_type application/json;
        return 200 '{"xff":"$http_x_forwarded_for","peer":"$http_x_test_observed_peer","forwarded":"$http_forwarded","host":"$http_x_forwarded_host","proto":"$http_x_forwarded_proto","port":"$http_x_forwarded_port","real":"$http_x_real_ip"}';
    }
}
'''
        controller.active.write_text(active)
        name = "sanvi-proxy-test-" + uuid.uuid4().hex[:10]
        env = dict(os.environ, DOCKER_API_VERSION="1.47")
        command = [DOCKER, "run", "-d", "--name", name, "-p", "127.0.0.1::8080",
                   "--mount", f"type=bind,source={ROOT / 'deployment/nginx.conf'},target=/etc/nginx/nginx.conf,readonly",
                   "--mount", f"type=bind,source={controller.runtime},target=/etc/nginx/sanvi,readonly",
                   "nginx:1.28-alpine"]
        try:
            started = subprocess.run(command, env=env, text=True, capture_output=True)
            self.assertEqual(started.returncode, 0, started.stderr)
            port = subprocess.run([DOCKER, "port", name, "8080/tcp"], env=env,
                                  capture_output=True, text=True, check=True).stdout.strip().rsplit(":", 1)[1]
            headers = {"X-Forwarded-For": "198.51.100.44, 203.0.113.77",
                       "Forwarded": "for=198.51.100.44;host=evil.invalid;proto=https",
                       "X-Forwarded-Host": "evil.invalid", "X-Forwarded-Proto": "evil",
                       "X-Forwarded-Port": "31337", "X-Real-IP": "198.51.100.44"}
            request = urllib.request.Request("http://127.0.0.1:" + port, headers=headers)
            for attempt in range(40):
                try:
                    with urllib.request.urlopen(request, timeout=2) as response:
                        body = json.load(response)
                    break
                except (urllib.error.URLError, ConnectionError):
                    if attempt == 39:
                        raise
                    time.sleep(0.1)
            self.assertEqual(body["xff"], body["peer"])
            ipaddress.ip_address(body["xff"])
            self.assertNotEqual(body["xff"], "198.51.100.44")
            for field in ("forwarded", "host", "proto", "port", "real"):
                self.assertEqual(body[field], "", field)
        finally:
            subprocess.run([DOCKER, "rm", "-f", name], env=env, capture_output=True, check=True)


if __name__ == "__main__":
    unittest.main()
