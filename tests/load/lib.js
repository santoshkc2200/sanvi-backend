// Shared harness for the TASK-019 k6 scenarios (FR-1103).
//
// Every scenario is one load shape, driven by `just perf`. Every run
// writes `benchmarks/<scenario>/<tag>.json` carrying the machine profile
// and the API build stamp (`GET /api/v1/system/build`), so two artifacts
// from different hosts or different builds are never silently compared
// (perf-compare refuses them).
//
// Lab honesty, stated once and inherited by every artifact: these are lab
// numbers. The local harness has no Ory Kratos and no Stripe, so the
// admin-write, checkout and webhook shapes measure the request path up to
// the authentication / signature boundary they legitimately stop at. The
// artifact records the observed status mix; nobody may read it as the
// end-to-end journey latency.

import http from 'k6/http';
import { Rate } from 'k6/metrics';
import { sleep } from 'k6';

// Lab expectation for the 401-boundary shapes (admin_write,
// checkout_create, webhook_flood): without a Kratos session or a Stripe
// secret the journey's legitimate stop is a 401. Scenarios add their
// responses to this rate with the statuses they expect, so the artifact's
// `expected_status_rate` means "share of responses within the lab
// contract" and a dip is an anomaly — http_req_failed cannot say that,
// because it counts every 401 as a failure.
export const expected_status = new Rate('expected_status');

// Scenarios call this with a response and the statuses its lab contract
// allows.
export function expect_status(res, ...allowed) {
  expected_status.add(allowed.includes(res.status));
}

export const BASE_URL = __ENV.PERF_BASE_URL || 'http://localhost:8081';
export const SEED = __ENV.PERF_SEED || '1';
export const TAG = __ENV.PERF_TAG || 'local';
export const SCENARIO = __ENV.PERF_SCENARIO || 'unknown';
export const MACHINE_PROFILE = JSON.parse(__ENV.PERF_MACHINE || '{}');
export const DURATION = __ENV.PERF_DURATION || '60s';
export const RATE = Number(__ENV.PERF_RATE || 0);

// Deterministic tenant slugs from the seed-scale distribution (seed 1
// unless PERF_SEED says otherwise): the three hot tenants plus a slice of
// the long tail, so a run sweeps data-shaped hosts, not one hot key.
export function tenant_slugs(count) {
  const slugs = ['scale-hot-audit', 'scale-hot-ads', 'scale-hot-payments'];
  const tail = Number(count || 12);
  for (let i = 0; i < tail; i += 1) {
    slugs.push(`scale-tail-${SEED}-${String(i).padStart(3, '0')}`);
  }
  return slugs;
}

// The tenant base domain in local development (configs/local.toml).
export function host_for(slug) {
  return `${slug}.localhost`;
}

// `GET /api/v1/system/build` (FR-1102): the stamp that names the build
// that produced every number in the artifact.
export function fetch_build_stamp() {
  const res = http.get(`${BASE_URL}/api/v1/system/build`);
  try {
    return res.json();
  } catch {
    return { error: `build probe failed: ${res.status}` };
  }
}

// Constant-arrival-rate executor at `rate` rps for the shared duration.
// The rate is the *offered* load; the artifact records what completed.
export function rate_scenario(rate) {
  return {
    executor: 'constant-arrival-rate',
    rate,
    timeUnit: '1s',
    duration: DURATION,
    preAllocatedVUs: Math.max(10, rate),
    maxVUs: rate * 4,
  };
}

// One summary entry per shape: the handful of metrics perf-compare diffs,
// plus the machine profile and build stamp that gate the comparison.
export function build_summary(data, extra) {
  const m = flatten_metrics(data.metrics);
  const build =
    (data.setup_data && data.setup_data.build) ||
    (globalThis && globalThis.__BUILD_STAMP) ||
    null;
  return JSON.stringify(
    {
      scenario: SCENARIO,
      tag: TAG,
      seed: SEED,
      offered_rate_rps: RATE || null,
      duration: DURATION,
      machine_profile: MACHINE_PROFILE,
      build,
      metrics: {
        http_req_duration_p50_ms: m.duration.med ?? null,
        http_req_duration_p95_ms: m.duration['p(0.95)'] ?? null,
        http_req_duration_p99_ms: m.duration['p(0.99)'] ?? null,
        http_req_duration_avg_ms: m.duration.avg ?? null,
        http_reqs_total: m.reqs ?? 0,
        // The unexpected-status rate: prefer the scenario's lab-contract
        // complement (1 - expected_status) when it defines one; otherwise
        // k6's http_req_failed (which counts every 4xx).
        http_req_failed_rate:
          m.expected == null ? (m.failed ?? 0) : 1 - m.expected,
        iterations_total: m.iterations ?? 0,
        dropped_iterations_total: m.dropped ?? 0,
      },
      ...extra,
    },
    null,
    2,
  );
}

// k6 nests trend/counter/rate numbers under `.values` in handleSummary;
// reach through defensively so a k6 version change degrades to nulls
// instead of crashing the summary.
function flatten_metrics(metrics) {
  const out = { duration: {}, reqs: 0, failed: 0, iterations: 0, dropped: 0, expected: null };
  if (!metrics) return out;
  const trend = metrics.http_req_duration;
  if (trend) out.duration = trend.values || trend;
  const reqs = metrics.http_reqs;
  if (reqs) out.reqs = (reqs.values && reqs.values.count) ?? reqs.count ?? 0;
  const failed = metrics.http_req_failed;
  if (failed) out.failed = (failed.values && failed.values.rate) ?? failed.value ?? 0;
  const iters = metrics.iterations;
  if (iters) out.iterations = (iters.values && iters.values.count) ?? iters.count ?? 0;
  const dropped =
    metrics.data_dropped_iterations || metrics.dropped_iterations;
  if (dropped) out.dropped = (dropped.values && dropped.values.count) ?? dropped.count ?? 0;
  const lab = metrics.expected_status;
  if (lab) out.expected = (lab.values && lab.values.rate) ?? lab.value ?? null;
  return out;
}

// Fixed think-time-free iterations: the shapes are arrival-driven.
export function no_sleep() {
  sleep(0);
}
