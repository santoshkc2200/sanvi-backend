// Load shape 5 of 5 (FR-1103): conversion capture — the storefront beacon
// (POST /api/v1/public/track) that feeds the conversion upload sweep. The
// k6 shape loads the HTTP capture front door; the upload itself is the
// worker's cross-tenant sweep (UploadPendingConversions), whose load is a
// function of captured volume, not of HTTP traffic — no k6 scenario can
// drive it directly, and this file does not pretend to.
//
// Lab caveat (see lib.js): the tracking site-key signing secret is empty
// locally, so the beacon's key verification fails closed and the response
// is a silent 202 (the beacon is deliberately indistinguishable to
// callers). The shape measures the front door: bot check, origin and
// per-IP/domain budgets, key verification.

import http from 'k6/http';
import {
  BASE_URL,
  SCENARIO,
  TAG,
  DURATION,
  RATE,
  build_summary,
  fetch_build_stamp,
  rate_scenario,
} from './lib.js';

export const options = {
  summaryTrendStats: ['med', 'avg', 'p(0.95)', 'p(0.99)'],
  scenarios: {
    conversion_upload: rate_scenario(Number(RATE || 50)),
  },
};

export function setup() {
  const build = fetch_build_stamp();
  globalThis.__BUILD_STAMP = build;
  return { build };
}

export default function () {
  http.post(
    `${BASE_URL}/api/v1/public/track`,
    JSON.stringify({
      event_id: `perf-${__VU}-${__ITER}`,
      event_name: 'Purchase',
      value_minor: 2500,
      currency: 'JPY',
      occurred_at: new Date().toISOString(),
    }),
    {
      headers: {
        'Content-Type': 'application/json',
        Origin: 'https://scale-hot-ads.domains.example',
        'X-Site-Key': 'not-a-real-key',
      },
    },
  );
}

export function handleSummary(data) {
  return {
    [`benchmarks/${SCENARIO}/${TAG}.json`]: build_summary(data, {
      notes: [
        'lab: unsigned beacon, so capture fails closed after key verification',
        'the beacon answers 202 in every case by design; upload is the worker sweep',
      ],
    }),
  };
}
