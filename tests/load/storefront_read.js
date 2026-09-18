// Load shape 1 of 5 (FR-1103): storefront read — the SSR bootstrap reads
// every anonymous storefront page renders: tenant resolution from the Host
// header plus the resolved public theme. Both are real, unauthenticated
// database-backed reads against scale tenants.

import http from 'k6/http';
import { check } from 'k6';
import {
  BASE_URL,
  SCENARIO,
  TAG,
  DURATION,
  build_summary,
  fetch_build_stamp,
  host_for,
  rate_scenario,
  tenant_slugs,
  RATE,
} from './lib.js';

const SLUGS = tenant_slugs(12);

export const options = {
  summaryTrendStats: ['med', 'avg', 'p(95)', 'p(99)'],
  scenarios: {
    storefront_read: rate_scenario(Number(RATE || 50)),
  },
  thresholds: {
    // The lab latency budget from the plan (a regression guard, not an
    // SLO): the storefront read journey budget is p95 ≤ 200 ms.
    http_req_duration: ['p(95)<200'],
  },
};

export function setup() {
  return { build: fetch_build_stamp() };
}

export default function () {
  // __VU phase-offsets the walk: __ITER alone steps every VU in
  // lockstep, pointing the whole offered rate at one tenant per instant.
  const slug = SLUGS[(__VU + __ITER) % SLUGS.length];
  const headers = { Host: host_for(slug) };

  const context = http.get(`${BASE_URL}/api/v1/public/tenant-context`, {
    headers,
  });
  check(context, { 'tenant-context answered 200': (r) => r.status === 200 });

  const theme = http.get(`${BASE_URL}/api/v1/public/theme`, { headers });
  check(theme, { 'theme answered under 500': (r) => r.status < 500 });
}

export function handleSummary(data) {
  return {
    [`benchmarks/${SCENARIO}/${TAG}.json`]: build_summary(data, {
      notes: [
        'lab: anonymous storefront SSR bootstrap (tenant-context + public theme)',
        'journey budget from the plan: p95 <= 200 ms',
      ],
    }),
  };
}
