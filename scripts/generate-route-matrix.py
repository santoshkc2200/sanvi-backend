#!/usr/bin/env python3
"""Generate the reviewed security inventory; this is not a Rust authorization proof.

PyYAML parses OpenAPI. Rust inspection accepts the repository's explicit utoipa
and Actix registration forms, and rejects unknown registered handler targets.
Security declarations live in route-metadata.json; source hashes make changes
review-visible even when the operation inventory does not change.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import sys

try:
    import yaml
except ImportError:
    raise SystemExit("route-matrix: install PyYAML==6.0.3")

METHODS = {"get", "post", "put", "patch", "delete", "head", "options", "trace"}

class UniqueLoader(yaml.SafeLoader):
    pass

def unique_mapping(loader, node, deep=False):
    result = {}
    for key_node, value_node in node.value:
        key = loader.construct_object(key_node, deep=deep)
        if key in result:
            raise ValueError(f"duplicate OpenAPI key: {key}")
        result[key] = loader.construct_object(value_node, deep=deep)
    return result

UniqueLoader.add_constructor(yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG, unique_mapping)

def source_files(root):
    files = list(root.glob("crates/contexts/*/src/adapters/http/**/*.rs"))
    files.extend(root.glob("apps/api/src/**/*.rs"))
    return sorted(set(files))

def closing_brace(text, start):
    # Ignore strings/comments while locating a Rust item's closing brace.
    tokens = re.finditer(r'//[^\n]*|/\*.*?\*/|"(?:\\.|[^"\\])*"|[{}]', text[start:], re.S)
    depth = 0
    for token in tokens:
        if token[0] == "{":
            depth += 1
        elif token[0] == "}":
            depth -= 1
            if depth == 0:
                return start + token.end()
    raise ValueError("unbalanced Rust item")

def production(text):
    while match := re.search(r"#\[cfg\(test\)\]", text):
        start = text.find("{", match.end())
        if start < 0:
            raise ValueError("unsupported test item")
        end = closing_brace(text, start)
        text = text[:match.start()] + "\n" * text[match.start():end].count("\n") + text[end:]
    return text

def runtime_registration(text, offset, relative):
    # All supported registrations have explicit web::<method>().to(handler).
    before = text[:offset]
    method = re.search(r"web::(get|post|put|patch|delete|head|options|trace)\(\)$", before)
    if not method:
        raise ValueError(f"unsupported runtime route method: {relative}")
    functions = list(re.finditer(r"\bfn\s+(\w+)\s*\(", before))
    if not functions:
        raise ValueError(f"runtime route outside function: {relative}")
    function = functions[-1]
    body = before[function.start():]
    # Catalog match arms name full paths (or identity/access relative paths).
    choices = []
    for pattern in (r'web::resource\("([^"\n]+)"\)',
                    r'\.route\("([^"\n]*)"\s*,',
                    r'"([^"\n]+)"\s*=>',
                    r'\("(?:GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS|TRACE)",\s*"([^"\n]+)"\)\s*=>'):
        choices.extend((match.start(), match[1]) for match in re.finditer(pattern, body))
    if not choices:
        raise ValueError(f"unsupported runtime route path: {relative}::{function[1]}")
    _, path = max(choices)
    scopes = list(re.finditer(r'web::scope\("([^"\n]+)"\)', body))
    scope = scopes[-1][1] if scopes else ""
    # Actix `{name:regex}` segments are OpenAPI's `{name}`.
    path = re.sub(r"\{(\w+):[^{}]*\}", r"{\1}", scope + path)
    return method[1].upper(), path, function[1]

def inspect_sources(root):
    handlers, annotated, registered = {}, {}, {}
    for path in source_files(root):
        text = production(path.read_text())
        if re.search(r'#\[(?:actix_web::)?(?:get|post|put|patch|delete|head|options|trace|route|routes)\s*\(', text):
            raise ValueError(f"unsupported Actix route macro: {path}")
        relative = path.relative_to(root).as_posix()
        context = relative.split("/")[2] if relative.startswith("crates/") else "app"
        for match in re.finditer(r"(?:pub(?:\([^)]*\))?\s+)?async\s+fn\s+(\w+)\s*\((.*?)\)\s*(?:->.*?)?\{", text, re.S):
            name, signature = match.groups()
            handlers[f"{relative}::{name}"] = signature
        for match in re.finditer(r"#\[utoipa::path\((.*?)\)\]", text, re.S):
            attr = match.group(1)
            method = re.search(r"^\s*(get|post|put|patch|delete|head|options|trace)\s*,", attr)
            route = re.search(r'path\s*=\s*"([^"\n]+)"', attr)
            function = re.search(r"(?:pub\s+)?async\s+fn\s+(\w+)", text[match.end():])
            if not (method and route and function):
                raise ValueError(f"unsupported utoipa declaration: {relative}")
            key = (method[1].upper(), route[1])
            if key in annotated:
                raise ValueError(f"duplicate handler route: {key}")
            annotated[key] = f"{relative}::{function[1]}"
        targets = re.findall(r"\.to\(\s*([\w:]+)\s*\)", text)
        if len(targets) != len(re.findall(r"\.to\(", text)):
            raise ValueError(f"unsupported registration target: {relative}")
        for target_match in re.finditer(r"\.to\(\s*([\w:]+)\s*\)", text):
            target = target_match[1]
            key = (context, target.split("::")[-1])
            if key in registered:
                raise ValueError(f"handler registered more than once; add explicit inventory support: {key}")
            registered[key] = runtime_registration(text, target_match.start(), relative)
    return handlers, annotated, registered

def openapi_routes(root):
    routes = {}
    docs = sorted((root / "api").glob("*.yaml"))
    if not docs:
        raise ValueError("no OpenAPI documents")
    for path in docs:
        document = yaml.load(path.read_text(), Loader=UniqueLoader)
        if not isinstance(document, dict) or not isinstance(document.get("paths"), dict):
            raise ValueError(f"invalid OpenAPI paths: {path}")
        for route, item in document["paths"].items():
            if not isinstance(item, dict):
                raise ValueError(f"invalid OpenAPI path: {route}")
            for method, operation in item.items():
                if method not in METHODS:
                    if method not in {"parameters", "summary", "description", "servers", "$ref"} and not method.startswith("x-"):
                        raise ValueError(f"unsupported OpenAPI method: {method}")
                    if method == "$ref":
                        raise ValueError(f"referenced paths unsupported: {route}")
                    continue
                key = (method.upper(), route)
                if key in routes or not isinstance(operation, dict) or not operation.get("operationId"):
                    raise ValueError(f"duplicate/invalid OpenAPI operation: {key}")
                routes[key] = operation["operationId"]
    if not routes:
        raise ValueError("no OpenAPI operations")
    return routes

def generate(root):
    root = Path(root)
    operations = openapi_routes(root)
    handlers, annotated, registered = inspect_sources(root)
    metadata = json.loads((root / "docs/security/route-metadata.json").read_text())
    rows = {}
    evidence = set()
    for row in metadata["routes"]:
        key = (row["method"], row["path"])
        if key in rows:
            raise ValueError(f"duplicate metadata: {key}")
        for field in ("permission", "tenant", "rate_limit", "handler", "evidence"):
            if not row.get(field) or (isinstance(row[field], str) and not row[field].strip()):
                raise ValueError(f"missing {field}: {key}")
        handler = row["handler"]
        if handler not in handlers:
            raise ValueError(f"missing handler: {handler}")
        signature = handlers[handler]
        evidence_paths = [root / item for item in row["evidence"]]
        if any(not path.is_file() or not path.resolve().is_relative_to(root.resolve()) for path in evidence_paths):
            raise ValueError(f"missing/invalid evidence: {key}")
        source = "\n".join(production(path.read_text()) for path in evidence_paths)
        permission = row["permission"]
        if not permission.startswith("exempt:"):
            if "RequiredPermission" not in signature:
                raise ValueError(f"missing RequiredPermission extractor: {key}")
            for evidence_path in evidence_paths:
                if evidence_path.name == "catalog.rs":
                    blocks = re.findall(r"RouteDef\s*\{(.*?)\n\s*\}", evidence_path.read_text(), re.S)
                    declared = [block for block in blocks if f'path: "{key[1]}"' in block and f'Method::{key[0]}' in block]
                    if len(declared) != 1 or f'"{permission}"' not in declared[0]:
                        raise ValueError(f"permission {permission} not declared for route: {key}")
            for evidence_path in evidence_paths:
                text = production(evidence_path.read_text())
                if "pub const PERMISSIONED_ROUTES" in text:
                    tuples = re.findall(r'\(\s*"([^"\n]+)"\s*,\s*Method::(\w+)\s*,\s*"([^"\n]+)"\s*,\s*ResourceSpec::', text, re.S)
                    declared = {(method, path): value for path, method, value in tuples}
                    if row.get("permission_declaration") != "direct" and declared.get(key) != permission:
                        raise ValueError(f"permission {permission} not in route declaration: {key}")
            if f'"{permission}"' not in source:
                raise ValueError(f"permission {permission} absent from evidence: {key}")
        if row["tenant"] == "CurrentTenant" and "CurrentTenant" not in signature:
            raise ValueError(f"missing CurrentTenant extractor: {key}")
        evidence.update(row["evidence"])
        evidence.add(handler.rsplit("::", 1)[0])
        rows[key] = row
    for key, operation in operations.items():
        if key not in rows:
            raise ValueError(f"unmapped OpenAPI route: {key}")
        if key not in annotated or annotated[key] != rows[key]["handler"]:
            raise ValueError(f"OpenAPI route lacks matching handler annotation: {key}")
        handler_file, handler_name = rows[key]["handler"].rsplit("::", 1)
        declaration = re.search(r'#\[utoipa::path\((.*?)\)\]\s*pub\s+async\s+fn\s+' + re.escape(handler_name) + r'\b', (root / handler_file).read_text(), re.S)
        explicit_id = re.search(r'operation_id\s*=\s*"([^"]+)"', declaration[1].rsplit("#[utoipa::path(", 1)[-1]) if declaration else None
        if (explicit_id[1] if explicit_id else handler_name) != operation:
            raise ValueError(f"operationId/handler mismatch: {key}")
    for key, handler in annotated.items():
        if key not in rows or rows[key]["handler"] != handler:
            raise ValueError(f"unmapped handler route: {key}")
        if key not in operations:
            raise ValueError(f"annotated route absent from OpenAPI: {key}")
    mapped_targets = set()
    for row in rows.values():
        relative, name = row["handler"].rsplit("::", 1)
        context = relative.split("/")[2] if relative.startswith("crates/") else "app"
        mapped_targets.add((context, name))
        if (context, name) not in registered:
            raise ValueError(f"metadata handler is not registered: {name}")
        method, local_path, function = registered[context, name]
        prefix = row.get("mount_prefix", "")
        if (method, prefix + local_path) != (row["method"], row["path"]):
            raise ValueError(f"runtime route differs from metadata: {method} {prefix + local_path} ({name})")
        if prefix:
            mount_evidence = row.get("mount_evidence", {})
            mount_source = root / mount_evidence.get("source", "")
            if not mount_source.is_file() or mount_evidence.get("expression", "INVALID") not in production(mount_source.read_text()):
                raise ValueError(f"runtime route mount evidence changed: {name}")
            evidence.add(mount_evidence["source"])
        if (row["method"], row["path"]) not in operations and not row.get("openapi_exemption"):
            raise ValueError(f"undocumented route lacks openapi_exemption: {name}")
    if set(registered) - mapped_targets:
        raise ValueError(f"unmapped registered handlers: {sorted(set(registered) - mapped_targets)}")
    lines = ["# Route security matrix", "", "Generated by `just route-matrix-write`; do not edit. Source: OpenAPI plus", "reviewed handler metadata in `route-metadata.json`. Every explicit exemption", "is a classification, not a claim that authorization or isolation is proven.", "", "Rate classes: `global-config-dependent` means the shared per-IP limiter is", "controlled by enabled/exempt_paths/fail_open configuration. `+tenant-optional`", "means tenant policy may add a limiter after resolution; neither implies a", "dedicated abuse quota. Additional controls are named explicitly in metadata.", "", "Abuse quotas (TASK-024, FR-1117): fixed per-IP/per-tenant windows under", "`server.abuse` count every money/reputation surface (tracking, invitation,", "domain purchase, signup webhook; notification sends count at the gateway);", "enforcement ships on for the public tracking beacon only — everywhere else", "the quota shadows and records would-block.", "", "Source hashes below make handler, registration, and middleware changes visible", "in this committed diff. Review metadata whenever these change; acknowledge", "the matrix diff in the PR description. Permission enforcement can be bypassed", "when identity wiring is disabled; a declared key does not imply enforcement", "under every feature/configuration combination. The gate checks declarations, not Rust", "semantics or runtime enforcement. Feature-gated routes are included even when", "disabled. Implicit framework HEAD/OPTIONS behavior is not a separate handler.", "", f"Routes: {len(rows)} ({len(operations)} OpenAPI operations).", "", "| Method | Path | Handler | Required permission / exemption | Tenant scoping | Rate-limit class | Contract |", "| --- | --- | --- | --- | --- | --- | --- |"]
    for key, row in sorted(rows.items(), key=lambda item: (item[0][1], item[0][0])):
        contract = "OpenAPI" if key in operations else row["openapi_exemption"]
        cells = [*key, row["handler"], row["permission"], row["tenant"], row["rate_limit"], contract]
        lines.append("| " + " | ".join(str(cell).replace("|", "&#124;").replace("\n", " ") for cell in cells) + " |")
    # Include shared enforcement and composition even if no row cites them.
    for pattern in ("crates/platform/http/src/*.rs", "crates/platform/http/src/tenancy/*.rs"):
        evidence.update(path.relative_to(root).as_posix() for path in root.glob(pattern))
    evidence.update(path.relative_to(root).as_posix() for path in source_files(root))
    lines += ["", "## Source fingerprints", "", "Review these sources alongside their affected rows; hashes exclude test modules.", "", "| Source | SHA-256 |", "| --- | --- |"]
    for relative in sorted(evidence):
        content = (root / relative).read_text()
        if relative == "crates/platform/http/src/middleware.rs":
            # This inventory concerns route/auth/rate controls, not CORS/header snapshots.
            content = content.split("fn build_cors", 1)[0] + content[content.index("struct RateLimit {"):]
        else:
            content = production(content)
        digest = hashlib.sha256(content.encode()).hexdigest()
        lines.append(f"| {relative} | `{digest}` |")
    return "\n".join(lines) + "\n"

def check(root):
    expected = generate(root)
    matrix = Path(root) / "docs/security/route-matrix.md"
    if not matrix.exists() or matrix.read_text() != expected:
        raise ValueError("stale route matrix; review metadata, run just route-matrix-write, acknowledge diff")

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    try:
        if args.write:
            (root / "docs/security/route-matrix.md").write_text(generate(root))
        else:
            check(root)
    except (ValueError, KeyError, OSError, yaml.YAMLError) as error:
        print(f"route-matrix: {error}", file=sys.stderr)
        return 1
    print("route-matrix: complete and current")
    return 0

if __name__ == "__main__":
    sys.exit(main())
