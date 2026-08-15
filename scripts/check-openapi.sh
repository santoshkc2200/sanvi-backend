#!/usr/bin/env bash
# Contract-first gate: the committed api/*.yaml specs must match the
# OpenAPI generated from code annotations.

set -euo pipefail

cd "$(dirname "$0")/.."

cargo run -p sanvi-cli -- openapi --check
