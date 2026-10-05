// Per-endpoint probe (TASK-021 step 2): one endpoint per run, configured
// entirely by PERF_PROBE_* env. The profile records in benchmarks/profiles/
// cite the artifact this writes as their before-number. The summary shape is
// the shared stats_version-2 one from lib.js, so perf-compare can diff two
// tags of the same probe; the machine profile and build stamp ride along
// like every other artifact.
//
// Run one probe:
//   k6 run -e PERF_SCENARIO=profiles/<name> -e PERF_PROBE_NAME=<name> \
//     -e PERF_PROBE_METHOD=GET -e PERF_PROBE_PATH=/api/v1/... \
//     [-e PERF_PROBE_HOST=tenant.localhost] [-e PERF_PROBE_BODY='{...}'] \
//     [-e PERF_PROBE_HEADERS='{"X-Site-Key":"k"}'] [-e PERF_PROBE_EXPECT=200,401] \
//     tests/load/profile_probe.js
//
// PERF_PROBE_NAME is the artifact directory under benchmarks/profiles/.
// PERF_PROBE_EXPECT is the lab contract for the endpoint (comma-separated
// statuses) feeding the expected_status rate, exactly like the 401-boundary
// shapes: an unauthenticated journey's legitimate stop is its boundary
// status, and the artifact records the observed mix.

import http from 'k6/http';
import {
  BASE_URL,
  TAG,
  RATE,
  build_summary,
  expect_status,
  fetch_build_stamp,
  rate_scenario,
} from './lib.js';

const NAME = __ENV.PERF_PROBE_NAME;
if (!NAME) {
  throw new Error('PERF_PROBE_NAME is required (artifact dir under benchmarks/profiles/)');
}

const METHOD = (__ENV.PERF_PROBE_METHOD || 'GET').toUpperCase();
const PATH = __ENV.PERF_PROBE_PATH || '/healthz';
const HOST = __ENV.PERF_PROBE_HOST || '';
const BODY = __ENV.PERF_PROBE_BODY || '';
const HEADERS = __ENV.PERF_PROBE_HEADERS ? JSON.parse(__ENV.PERF_PROBE_HEADERS) : {};
const EXPECTED = (__ENV.PERF_PROBE_EXPECT || '')
  .split(',')
  .map((s) => Number(s.trim()))
  .filter((n) => n > 0);

export const options = {
  summaryTrendStats: ['med', 'avg', 'p(95)', 'p(99)'],
  scenarios: {
    [NAME]: rate_scenario(Number(RATE || 20)),
  },
};

export function setup() {
  return { build: fetch_build_stamp() };
}

export default function () {
  const headers = Object.assign({}, HEADERS);
  if (HOST) headers.Host = HOST;
  if (BODY && METHOD !== 'GET' && !headers['Content-Type']) {
    headers['Content-Type'] = 'application/json';
  }
  const res = http.request(METHOD, `${BASE_URL}${PATH}`, BODY || null, { headers });
  if (EXPECTED.length) {
    expect_status(res, ...EXPECTED);
  }
}

export function handleSummary(data) {
  return {
    [`benchmarks/profiles/${NAME}/${TAG}.json`]: build_summary(data, {
      notes: [
        `probe: ${METHOD} ${PATH}${HOST ? ` (Host: ${HOST})` : ''}`,
        __ENV.PERF_PROBE_NOTE || 'TASK-021 step 2: per-endpoint before-number artifact',
      ],
    }),
  };
}
