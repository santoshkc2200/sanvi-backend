#!/usr/bin/env bash
# Scaffold a new bounded context with the four-layer hexagonal skeleton.
#
#   ./scripts/new-context.sh billing
#
# Creates crates/contexts/<name> with domain/application/adapters modules,
# registers it in the workspace, and prints the wiring recipe.

set -euo pipefail

cd "$(dirname "$0")/.."

NAME="${1:?usage: $0 <context-name>}"
PASCAL="${NAME^}"
CRATE_DIR="crates/contexts/${NAME}"

if [[ -d "${CRATE_DIR}" ]]; then
    echo "context ${NAME} already exists" >&2
    exit 1
fi

mkdir -p "${CRATE_DIR}/src/domain" "${CRATE_DIR}/src/application" "${CRATE_DIR}/src/adapters/http"

cat > "${CRATE_DIR}/Cargo.toml" <<EOF
[package]
name = "sanvi-${NAME}"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
actix-web.workspace = true
async-trait.workspace = true
sanvi-http.workspace = true
sanvi-kernel.workspace = true
sanvi-persistence.workspace = true
serde.workspace = true
thiserror.workspace = true
tokio.workspace = true
utoipa.workspace = true

[dev-dependencies]
sanvi-kernel.workspace = true
EOF

cat > "${CRATE_DIR}/src/lib.rs" <<EOF
//! \`${NAME}\` context.

pub mod adapters;
pub mod application;
pub mod domain;
EOF

cat > "${CRATE_DIR}/src/domain/mod.rs" <<EOF
//! Domain: entities, value objects, events, errors, ports.
//! No infrastructure imports allowed here (enforced by scripts/check-layering.sh).

pub mod error;
pub mod ports;

pub use error::${PASCAL}Error;
EOF

cat > "${CRATE_DIR}/src/domain/error.rs" <<EOF
use thiserror::Error;

/// Domain errors for the \`${NAME}\` context.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ${PASCAL}Error {
    #[error("not implemented")]
    NotImplemented,
}
EOF

cat > "${CRATE_DIR}/src/domain/ports.rs" <<EOF
//! Ports (traits) implemented by driven adapters.
EOF

cat > "${CRATE_DIR}/src/application/mod.rs" <<EOF
//! Application: use cases, services, DTOs.
EOF

cat > "${CRATE_DIR}/src/adapters/mod.rs" <<EOF
//! Adapters: infrastructure (driven) and transport (driving).
pub mod http;
EOF

cat > "${CRATE_DIR}/src/adapters/http/mod.rs" <<EOF
//! HTTP driving adapter: handlers + routes + OpenAPI.

use utoipa::OpenApi;

/// OpenAPI document contributed by this context.
#[derive(OpenApi)]
pub struct ${PASCAL}ApiDoc;
EOF

# Register in the workspace.
python3 - "${NAME}" <<'PY'
import sys
name = sys.argv[1]
path = "Cargo.toml"
text = open(path).read()
anchor = '    "crates/contexts/system",'
addition = f'    "crates/contexts/{name}",'
assert anchor in text, "workspace members anchor not found"
text = text.replace(anchor, anchor + "\n" + addition, 1)
open(path, "w").write(text)
print(f"registered crates/contexts/{name} in workspace members")
PY

echo
echo "Created ${CRATE_DIR}."
echo "Next steps (the 'how to add a use case' recipe lives in CONTRIBUTING.md):"
echo "  1. Add the domain vocabulary under src/domain/"
echo "  2. Add use cases under src/application/"
echo "  3. Add handlers/routes under src/adapters/http/ and export the ApiDoc"
echo "  4. Mount routes + ApiDoc in apps/api/src/lib.rs"
echo "  5. Commit api/${NAME}.yaml (run: cargo run -p sanvi-cli -- openapi)"
