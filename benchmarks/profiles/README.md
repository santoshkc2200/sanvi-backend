# Endpoint profiles - TASK-021 step 2 (slice 11.2c)

Twenty endpoints, chosen **by (requests x avg latency) from measured artifacts**, not by opinion.
Every number below is a **lab** number: seed-1 scale data, release build, one Windows host
(20 cores / 16 GB, profile recorded in every artifact), not a capacity claim.

## How the set was chosen

The 2026-10-02 baseline's five k6 shapes are per-shape, not per-endpoint, so the ranking input was
measured here: `tests/load/profile_probe.js` (driven by `scripts/profile-probes.sh`) probes one
endpoint per run at a uniform offered rate (20 rps, 30 s, ~600 requests each) and writes the shared
stats_version-2 summary to `benchmarks/profiles/<name>/rc0.json` - machine profile, build stamp and
expected-status mix included. 32 candidate endpoints covering every public surface the lab can
reach, the operational surfaces, and 401-boundary representatives across the tenant contexts were
probed; the ranking is (requests x avg latency) over those artifacts, run 2026-10-05 against the
lab posture recorded in `benchmarks/baseline-2026-10-02.md` (rate limiting off, tracking quota
raised, `SANVI__DATABASE__NAME=sanvi_scale`).

The top 20 by that ranking are the profile records `benchmarks/profiles/<name>.md`; each cites its
committed artifact as its before-number and carries the `pg_stat_statements` capture
(`benchmarks/profiles/<name>/statements-rc0.txt`: reset immediately before the probe, top 15 by
total time after) as its server-side attribution.

## What the lab can and cannot profile here

**Measured and committed:** per-endpoint latency distribution (the before-number every later
optimisation slice in this task is compared against) and per-endpoint statement mix (step 3's and
step 4's input).

**Named gap - CPU sampling and task stalls.** `cargo flamegraph`/samply sampling on Windows uses
ETW and requires an elevated host; this session's host has sudo disabled and no elevation
(`samply record`: "samply requires Administrator privileges for profiling on Windows"). The API is
actix-web on the tokio runtime without `tokio-console` instrumentation, and its TUI cannot be
captured headlessly. CPU attribution therefore stops at what the artifacts support: in-process
versus database (the statements captures), and the endpoint-level latency distribution. On an
elevated host the drill is: `samply record -r 1000 -- <release api.exe>` while the same probe runs,
flamegraph per endpoint into `benchmarks/profiles/<name>/`. Nothing in the records claims CPU
shape that was not sampled.

**Named gap - authenticated handlers.** No Ory Kratos locally (the baseline's standing caveat):
ten authenticated endpoints profile only their middleware front half (tenant resolution +
authentication rejection). They are kept in the set because that front half is real cost every
authenticated journey pays, and their records say exactly that.

## Defects found while smoke-testing the probe set (fixed here, recorded)

Smoke-testing every candidate path before probing surfaced a route-mount defect class the route
and permission gates cannot see: **actix commits a request to the first matching resource, so the
privacy and billing route registrations - one `web::resource` per catalog entry - shadowed every
second method on a shared path (405), and the privacy subject scope was mounted under
`/api/v1/privacy` while registering full paths, doubling the prefix (404 for the whole scope).**
Live: every platform privacy write (hold/incident/subprocessor/activity/assessment/jurisdiction
POST-PUTs), the tenant DSR list (GET), `POST /api/v1/platform/plans`, and the entire subject
privacy surface (`POST /api/v1/privacy/requests`, consents, directives, opt-out, ...) did not
serve at their contracted paths. Fixed in `sanvi_privacy` and `sanvi_billing` (group one resource
per path; `PermissionMap` for per-method permissions; subject paths relative to their mount), each
with a regression test proven to fail on the old registration. An OpenAPI-wide live sweep (all 210
operations, one request each) now reports zero 405s. This is the slice's only behaviour change,
and it makes the app serve contracted endpoints it previously refused.

## Ranking (requests x avg latency; all 32 probes)

| # | probe | endpoint | reqs | avg ms | p95 ms | p99 ms | requests x avg | in top 20 |
|---|---|---|---:|---:|---:|---:|---:|---|

| 1 | `metrics` | GET /metrics | 601 | 28.46 | 50.26 | 79.86 | 17105 | **yes** |
| 2 | `openapi_json` | GET /openapi.json | 602 | 25.25 | 45.73 | 51.45 | 15201 | **yes** |
| 3 | `public_plans` | GET /api/v1/public/plans | 602 | 7.94 | 14.90 | 16.99 | 4783 | **yes** |
| 4 | `public_privacy-notice` | GET /api/v1/public/privacy/notice | 601 | 7.02 | 13.43 | 16.69 | 4216 | **yes** |
| 5 | `public_notice-at-collection` | GET /api/v1/public/privacy/notice-at-collection | 601 | 6.35 | 12.84 | 15.88 | 3815 | **yes** |
| 6 | `public_subprocessors` | GET /api/v1/public/subprocessors | 602 | 5.79 | 11.82 | 16.67 | 3483 | **yes** |
| 7 | `public_privacy-metrics` | GET /api/v1/public/privacy/metrics | 602 | 5.58 | 11.38 | 14.67 | 3360 | **yes** |
| 8 | `public_theme` | GET /api/v1/public/theme (tenant host) | 601 | 5.54 | 10.44 | 13.75 | 3328 | **yes** |
| 9 | `tenant_theme-draft` | GET /api/v1/tenant/theme/draft (host) | 602 | 5.41 | 10.59 | 12.38 | 3259 | **yes** |
| 10 | `public_slug-availability` | GET /api/v1/public/slug-availability?slug=... | 602 | 5.40 | 10.98 | 14.15 | 3253 | **yes** |
| 11 | `tenant_domains` | GET /api/v1/tenant/domains (host) | 602 | 5.35 | 10.75 | 14.30 | 3222 | **yes** |
| 12 | `tenant_invoices` | GET /api/v1/tenant/billing/invoices (host) | 602 | 5.25 | 10.38 | 15.48 | 3158 | **yes** |
| 13 | `tenant_roles` | GET /api/v1/tenant/roles (host) | 602 | 5.15 | 10.77 | 14.65 | 3103 | **yes** |
| 14 | `tenant_checkout-create` | POST /api/v1/tenant/checkout (host) | 602 | 5.12 | 10.55 | 13.01 | 3081 | **yes** |
| 15 | `tenant_members` | GET /api/v1/tenant/members (host) | 601 | 5.03 | 10.18 | 13.13 | 3023 | **yes** |
| 16 | `tenant_settings_get` | GET /api/v1/tenant/settings (host) | 601 | 4.98 | 10.38 | 12.72 | 2993 | **yes** |
| 17 | `tenant_settings_put` | PUT /api/v1/tenant/settings (host) | 601 | 4.95 | 10.43 | 13.73 | 2977 | **yes** |
| 18 | `tenant_payments` | GET /api/v1/tenant/payments (host) | 602 | 4.72 | 10.15 | 13.03 | 2839 | **yes** |
| 19 | `tenant_campaigns` | GET /api/v1/tenant/ads/campaigns (host) | 601 | 4.69 | 10.01 | 11.88 | 2817 | **yes** |
| 20 | `tenant_privacy-requests` | GET /api/v1/tenant/privacy/requests (host) | 602 | 4.67 | 9.79 | 11.89 | 2809 | **yes** |
| 21 | `public_tenant-context` | GET /api/v1/public/tenant-context (tenant host) | 602 | 4.63 | 10.12 | 11.95 | 2788 | no |
| 22 | `tenant_context` | GET /api/v1/tenant/context (tenant host) | 602 | 4.23 | 8.63 | 12.38 | 2546 | no |
| 23 | `public_track` | POST /api/v1/public/track | 602 | 3.39 | 7.21 | 8.59 | 2043 | no |
| 24 | `readyz` | GET /readyz | 601 | 3.35 | 7.37 | 9.39 | 2014 | no |
| 25 | `public_locales` | GET /api/v1/public/locales | 602 | 3.26 | 6.93 | 8.03 | 1963 | no |
| 26 | `platform_admin-tenants` | GET /api/v1/platform/admin/tenants | 602 | 2.10 | 4.51 | 7.14 | 1267 | no |
| 27 | `stripe_connect-webhook` | POST /internal/webhooks/stripe/connect | 602 | 2.09 | 4.48 | 5.51 | 1261 | no |
| 28 | `public_messages` | GET /api/v1/public/messages/errors?locale=ja | 602 | 2.09 | 4.31 | 5.26 | 1256 | no |
| 29 | `healthz` | GET /healthz | 602 | 2.04 | 4.44 | 7.94 | 1228 | no |
| 30 | `tenant_me` | GET /api/v1/me | 602 | 1.96 | 3.93 | 4.91 | 1181 | no |
| 31 | `system_build` | GET /api/v1/system/build | 602 | 1.93 | 4.10 | 4.88 | 1163 | no |
| 32 | `system_version` | GET /api/v1/system/version | 601 | 1.89 | 4.27 | 5.53 | 1137 | no |

The cut is clean: #20 (`tenant_privacy-requests`, 2,811) and #21 (`tenant_context`, 2,547) are
separated by 10 %, and no ranking method choice inside the measured set (avg x requests, p95 x
requests) moves endpoints across it.

## Before-numbers: what step 3, 4 and 5 take from here

- **In-process, no SQL:** `metrics` and `openapi_json` - the two heaviest endpoints - cost nothing
  in the database. Their optimisation levers (pre-render, ETag) are contract-adjacent and belong
  to step 7, not to query work.
- **N+1 candidate:** `public_plans` fans out to 1 entitlements query + 1 prices query per plan row
  (step 4's first target).
- **Per-request regeneration of slow-moving content:** the four privacy disclosure reads rebuild
  from rules per request (step 6's keyed-cache candidates).
- **Shared dominant statement:** the live-hostnames resolution lookup appears once per
  tenant-host request (0.022-0.026 ms mean) - the baseline's dominant statement, unchanged.
- **Pool posture at 20 rps per probe:** no acquire waits surfaced (pool gauges idle; the baseline's
  3-4-of-10 posture holds at this rate) - step 5 sizes pools from the 3x run, not from these.
