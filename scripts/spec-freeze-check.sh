#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export PYTHONDONTWRITEBYTECODE=1
if [[ "${1:-}" == "--self-test" ]]; then exec python3 scripts/test-api-contract.py; fi
exec python3 scripts/api-contract.py freeze "$@"
