#!/usr/bin/env python3
"""Mutation tests exercise the production gate against isolated route trees."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

MODULE = Path(__file__).with_name("generate-route-matrix.py")

class RouteMatrixTests(unittest.TestCase):
    def test_gate_mutations(self):
        self.assertTrue(MODULE.exists(), "route matrix gate has not been implemented")
        spec = importlib.util.spec_from_file_location("route_matrix", MODULE)
        gate = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(gate)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            handler = root / "crates/contexts/demo/src/adapters/http/handlers.rs"
            handler.parent.mkdir(parents=True)
            handler.write_text('#[utoipa::path(get, path = "/demo")]\npub async fn demo(_guard: RequiredPermission, tenant: CurrentTenant) {}\n')
            routes = handler.with_name("routes.rs")
            routes.write_text('pub fn routes() { web::resource("/demo").app_data(require_permission!("demo.read", tenant)).route(web::get().to(demo)); }\n')
            api = root / "api/demo.yaml"
            api.parent.mkdir()
            api.write_text('openapi: 3.1.0\npaths:\n  /demo:\n    get:\n      operationId: demo\n')
            meta = root / "docs/security/route-metadata.json"
            meta.parent.mkdir(parents=True)
            metadata = {"routes": [{"method": "GET", "path": "/demo", "handler": handler.relative_to(root).as_posix() + "::demo", "permission": "demo.read", "tenant": "CurrentTenant", "rate_limit": "global-config-dependent", "evidence": [routes.relative_to(root).as_posix()]}]}
            meta.write_text(json.dumps(metadata))
            output = gate.generate(root)
            self.assertIn("demo.read", output)
            for field in ("permission", "tenant", "rate_limit"):
                with self.subTest(missing=field):
                    value = metadata["routes"][0].pop(field)
                    meta.write_text(json.dumps(metadata))
                    with self.assertRaisesRegex(ValueError, field):
                        gate.generate(root)
                    metadata["routes"][0][field] = value
            meta.write_text(json.dumps(metadata))
            original = api.read_text()
            api.write_text(original + '  /unmapped:\n    post:\n      operationId: unmapped\n')
            with self.assertRaisesRegex(ValueError, "unmapped"):
                gate.generate(root)
            api.write_text(original)
            original_handler = handler.read_text()
            handler.write_text(original_handler.replace("_guard: RequiredPermission, ", ""))
            with self.assertRaisesRegex(ValueError, "RequiredPermission"):
                gate.generate(root)
            handler.write_text(original_handler)
            original_routes = routes.read_text()
            declaration = '\npub const PERMISSIONED_ROUTES: &[Entry] = &[("/demo", Method::GET, "demo.read", ResourceSpec::Tenant)];\n'
            routes.write_text(original_routes + declaration)
            gate.generate(root)
            routes.write_text(original_routes + '\npub const PERMISSIONED_ROUTES: &[Entry] = &[];\n')
            with self.assertRaisesRegex(ValueError, "route declaration"):
                gate.generate(root)
            routes.write_text(original_routes)
            routes.write_text(original_routes.replace('"demo.read"', '"demo.write"'))
            with self.assertRaisesRegex(ValueError, "demo.read"):
                gate.generate(root)
            for changed in (original_routes.replace('"/demo"', '"/hidden"'), original_routes.replace('web::get()', 'web::post()')):
                routes.write_text(changed)
                with self.assertRaisesRegex(ValueError, "runtime route"):
                    gate.generate(root)
            routes.write_text(original_routes + '\n#[actix_web::post("/hidden")] async fn hidden() {}\npub fn hidden_routes(cfg: &mut web::ServiceConfig) { cfg.service(hidden); }')
            with self.assertRaisesRegex(ValueError, "unsupported.*macro"):
                gate.generate(root)
            routes.write_text(original_routes + '\n#[actix_web::trace("/hidden")] async fn hidden() {}\npub fn hidden_routes(cfg: &mut web::ServiceConfig) { cfg.service(hidden); }')
            with self.assertRaisesRegex(ValueError, "unsupported.*macro"):
                gate.generate(root)
            routes.write_text(original_routes)
            nested = handler.parent / "nested/hidden.rs"
            nested.parent.mkdir()
            nested.write_text('pub fn register() { web::resource("/hidden").route(web::post().to(hidden)); } async fn hidden() {}')
            with self.assertRaisesRegex(ValueError, "hidden"):
                gate.generate(root)
            nested.unlink()
            routes.write_text(original_routes + '\n#[cfg(test)] mod tests {}\npub fn late() { web::resource("/late").route(web::post().to(late)); } async fn late() {}')
            with self.assertRaisesRegex(ValueError, "late"):
                gate.generate(root)
            routes.write_text(original_routes)
            routes.write_text(original_routes + '\npub fn added() { web::resource("/hidden").route(web::post().to(hidden)); }')
            with self.assertRaisesRegex(ValueError, "hidden"):
                gate.generate(root)
            routes.write_text(original_routes + '\nweb::resource("/alias").route(web::get().to(demo));')
            with self.assertRaisesRegex(ValueError, "registered more than once"):
                gate.generate(root)
            routes.write_text(original_routes)
            api.write_text(original + '  /demo:\n    post:\n      operationId: duplicate\n')
            with self.assertRaisesRegex(ValueError, "duplicate OpenAPI key"):
                gate.generate(root)
            api.write_text(original)
            self.assertEqual(output, gate.generate(root))
            matrix = root / "docs/security/route-matrix.md"
            matrix.write_text(output + "stale")
            with self.assertRaisesRegex(ValueError, "stale"):
                gate.check(root)
            matrix.write_text(output)
            gate.check(root)

if __name__ == "__main__":
    unittest.main()
