// Load shape 4 of 5 (FR-1103): webhook flood — Stripe's connected-account
// receiver (POST /internal/webhooks/stripe/connect) under a flood of
// events. The receiver is the money path: signature verification on the
// raw body runs before anything else, and fail-closed is the only
// behaviour.
//
// Lab caveat (see lib.js): no Stripe secret is configured locally, so
// every event in this shape is rejected at signature verification (401).
// That is still the receiver's most traffic-sensitive code path — the HMAC
// over the raw body — and the artifact records the status mix.

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
  rate_scenario,
} from './lib.js';

export const options = {
  summaryTrendStats: ['med', 'avg', 'p(95)', 'p(99)'],
  scenarios: {
    webhook_flood: rate_scenario(Number(RATE || 100)),
  },
};

export function setup() {
  return { build: fetch_build_stamp() };
}

export default function () {
  const payload = JSON.stringify({
    id: `evt_perf_${__VU}_${__ITER}`,
    type: 'charge.succeeded',
    created: Math.floor(Date.now() / 1000),
    data: { object: { id: `pi_perf_${__ITER}` } },
  });
  const res = http.post(`${BASE_URL}/internal/webhooks/stripe/connect`, payload, {
    headers: {
      'Content-Type': 'application/json',
      // A syntactically valid Stripe signature header with a bogus key:
      // the receiver must verify it and fail closed.
      'Stripe-Signature': `t=${Math.floor(Date.now() / 1000)},v1=${'0'.repeat(64)}`,
    },
  });
  expect_status(res, 200, 401);
}

export function handleSummary(data) {
  return {
    [`benchmarks/${SCENARIO}/${TAG}.json`]: build_summary(data, {
      notes: [
        'lab: unsigned events, so the receiver rejects at signature verification',
        'the verified-HMAC-over-raw-body path is the shape under load',
      ],
    }),
  };
}
