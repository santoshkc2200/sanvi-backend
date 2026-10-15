#!/usr/bin/env python3
"""Exercise the production Caddy routes with a local CA and deterministic gate.

Requires Docker and Python 3. Uses a disposable container, localhost-only random
port and local certificates; never contacts ACME or a deployment environment.
"""

import http.client
from pathlib import Path
import ssl
import subprocess
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]
IMAGE = "caddy:2.11.2-alpine"


def docker(*args):
    return subprocess.check_output(["docker", *args], text=True).strip()


def main():
    with tempfile.TemporaryDirectory(prefix="sanvi-domains-edge-") as directory:
        fixtures = Path(directory)
        enabled = fixtures / "enabled"
        enabled.touch()
        production = (ROOT / "deployment/Caddyfile.domains").read_text()
        # Keep the real routes and ask bridge; replace only the issuer.
        config = "\n".join(
            "    cert_issuer internal" if line.strip().startswith("acme_ca ") else line
            for line in production.splitlines()
        ) + "\n"
        config += """
http://:9081 {
    root * /fixtures
    @allowed {
        query host=shop.example.com
        header x-sanvi-internal edge-test
        file enabled
    }
    respond @allowed 200
    respond 403
}
http://:9082 {
    respond "{host}|{header.x-sanvi-internal}" 200
}
"""
        (fixtures / "Caddyfile").write_text(config)
        container = docker(
            "run", "-d", "-p", "127.0.0.1::443",
            "-e", "ACME_EMAIL=domains@example.com",
            "-e", "API_UPSTREAM=127.0.0.1:9081",
            "-e", "STOREFRONT_UPSTREAM=127.0.0.1:9082",
            "-e", "SANVI_DOMAIN_INTERNAL_SECRET=edge-test",
            "-v", f"{fixtures}:/fixtures:ro", IMAGE,
            "caddy", "run", "--config", "/fixtures/Caddyfile", "--adapter", "caddyfile",
        )
        try:
            port = int(docker("port", container, "443/tcp").rsplit(":", 1)[1])
            root = fixtures / "root.crt"
            for attempt in range(50):
                result = subprocess.run(
                    ["docker", "cp", f"{container}:/data/caddy/pki/authorities/local/root.crt", str(root)],
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                )
                if result.returncode == 0:
                    break
                time.sleep(0.1)
            else:
                raise AssertionError("local CA did not initialize")
            context = ssl.create_default_context(cafile=str(root))

            def request(host, path="/", headers=None, server_name=None):
                # Connect locally while validating the customer's SNI and cert.
                import socket
                with socket.create_connection(("127.0.0.1", port), timeout=5) as tcp:
                    with context.wrap_socket(tcp, server_hostname=server_name or host) as tls:
                        extra = "".join(f"{key}: {value}\r\n" for key, value in (headers or {}).items())
                        tls.sendall(f"GET {path} HTTP/1.1\r\nHost: {host}\r\n{extra}Connection: close\r\n\r\n".encode())
                        response = http.client.HTTPResponse(tls)
                        response.begin()
                        return response.status, response.read().decode()

            assert request("shop.example.com", headers={"x-sanvi-internal": "attacker"}) == (200, "shop.example.com|")
            assert request("shop.example.com", "/internal/tls/authorize")[0] == 404
            assert request("other.example.com", server_name="shop.example.com")[0] == 421
            try:
                request("unknown.example.com")
            except ssl.SSLError:
                pass
            else:
                raise AssertionError("unknown domain received a certificate")
            enabled.unlink()
            assert request("shop.example.com")[0] == 403, "cached certificate bypassed removed-domain gate"
            print("domain edge: OK (trusted TLS, ask translation, Host preservation, secret stripping, internal isolation, unknown/removed denial)")
        except Exception:
            print(docker("logs", container))
            raise
        finally:
            docker("rm", "-f", container)


if __name__ == "__main__":
    main()
