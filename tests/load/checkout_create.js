// Load shape 3 of 5 (FR-1103): checkout create — the tenant storefront's
// buy path (POST /api/v1/tenant/payments/checkout).
//
// Lab caveat (see lib.js): no Kratos session and no live Stripe exist in
// the harness, so the shape measures the path up to the authentication
// boundary (401). The plan's success-ratio budget (≥ 99.5 % excluding
// provider declines) is meaningful only once sessions exist; the artifact
// records the observed status mix so the day the harness gains a session,
// the same scenario measures the whole journey.

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

const SLUGS = tenant_slugs(4);

export const options = {
  summaryTrendStats: ['med', 'avg', 'p(95)', 'p(99)'],
  scenarios: {
    checkout_create: rate_scenario(Number(RATE || 10)),
  },
};

export function setup() {
  return { build: fetch_build_stamp() };
}

export default function () {
  // __VU phase-offsets the walk: __ITER alone steps every VU in
  // lockstep, pointing the whole offered rate at one tenant per instant.
  const slug = SLUGS[(__VU + __ITER) % SLUGS.length];
  const res = http.post(
    `${BASE_URL}/api/v1/tenant/checkout`,
    JSON.stringify({
      reference: `perf-${__VU}-${__ITER}`,
      amount_minor: 1000 + (__ITER % 90) * 10,
      currency: 'JPY',
      success_url: 'https://scale-hot-ads.domains.example/thanks',
      cancel_url: 'https://scale-hot-ads.domains.example/cart',
    }),
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
        'lab: no Kratos/Stripe in the harness, so the create stops at the 401 boundary',
        'plan budget: success ratio >= 99.5 % excluding provider declines (needs sessions)',
      ],
    }),
  };
}
