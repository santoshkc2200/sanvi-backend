// Load shape 2 of 5 (FR-1103): admin API writes — the tenant admin's
// settings PUT is a real write endpoint on the real middleware path.
//
// Lab caveat (see lib.js): the local harness has no Ory Kratos, so without
// a session the request stops at the authentication extractor (401). The
// shape therefore measures the front half of the journey — resolution,
// authn middleware, permission gate entry — and the artifact records the
// observed status mix. Nobody may read this as end-to-end write latency.

import http from 'k6/http';

import {
  BASE_URL,
  SCENARIO,
  TAG,
  DURATION,
  RATE,
  build_summary,
  expect_status,
  fetch_build_stamp,
  host_for,
  rate_scenario,
  tenant_slugs,
} from './lib.js';

const SLUGS = tenant_slugs(6);

export const options = {
  summaryTrendStats: ['med', 'avg', 'p(0.95)', 'p(0.99)'],
  scenarios: {
    admin_write: rate_scenario(Number(RATE || 20)),
  },
  thresholds: {
    // Admin API writes: the plan's journey budget is p95 ≤ 400 ms.
    http_req_duration: ['p(95)<400'],
  },
};

export function setup() {
  const build = fetch_build_stamp();
  globalThis.__BUILD_STAMP = build;
  return { build };
}

export default function () {
  const slug = SLUGS[__ITER % SLUGS.length];
  const res = http.put(
    `${BASE_URL}/api/v1/tenant/settings`,
    JSON.stringify({ settings: { 'perf.write': String(__ITER) } }),
    {
      headers: {
        'Content-Type': 'application/json',
        Host: host_for(slug),
      },
    },
  );
  expect_status(res, 200, 401);
}

export function handleSummary(data) {
  return {
    [`benchmarks/${SCENARIO}/${TAG}.json`]: build_summary(data, {
      notes: [
        'lab: no Kratos in the harness, so the write stops at the 401 boundary',
        'journey budget from the plan: p95 <= 400 ms',
      ],
    }),
  };
}
