#!/usr/bin/env bash
# TASK-022 11.3g lab launch: the TASK-021 soak posture
# (benchmarks/soak-2026-10-07/soak.sh) plus the platform.ssr_cache flag ON.
# The only deltas from the soak posture are the port override and
# SANVI__SERVER__SSR_CACHE__ENABLED=true.
set -uo pipefail
cd "$(dirname "$0")/../.."

export SANVI_ENV=local
export SANVI__DATABASE__NAME=sanvi_scale
export SANVI__DATABASE__PORT=5433
export SANVI__DATABASE__PASSWORD=app-local-dev
export SANVI__SERVER__PORT=8081
export SANVI__SERVER__RATE_LIMIT__ENABLED=false
export SANVI__SERVER__TENANT__TENANT_RATE_LIMIT__ENABLED=false
export SANVI__FEATURES__ADVERTISING=true
export SANVI__ADVERTISING__TRACKING_SITE_KEY_SIGNING_SECRET=lab-dev-tracking-site-key-signing-secret-0123456789
export SANVI__ADVERTISING__OAUTH_STATE_SIGNING_SECRET=lab-dev-oauth-state-signing-secret-0123456789
# base64 of the 32-byte lab token KEK (dev only)
export SANVI__ADVERTISING__TOKEN_KEK=bGFiLWRldi10b2tlbi1rZWstMDEyMzQ1Njc4OWFiY2Q=
export SANVI__SERVER__ABUSE__TRACKING__PER_IP__LIMIT=10000000
export SANVI__SERVER__ABUSE__TRACKING__PER_ORIGIN__LIMIT=10000000
export SANVI__SERVER__SSR_CACHE__ENABLED=true

exec ./target/release/sanvi-api.exe
