#!/usr/bin/env bash
# Seed reference data (phase 01+; nothing to seed in phase 00).

set -euo pipefail

cd "$(dirname "$0")/.."

cargo run -p sanvi-cli -- seed
