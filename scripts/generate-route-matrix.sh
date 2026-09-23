#!/usr/bin/env bash
set -euo pipefail
export PYTHONDONTWRITEBYTECODE=1
script_dir="$(dirname "${BASH_SOURCE[0]}")"
if [[ "${1:-}" == "--self-test" ]]; then
    exec python3 "$script_dir/test-route-matrix.py"
fi
exec python3 "$script_dir/generate-route-matrix.py" "$@"
