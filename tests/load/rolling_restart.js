// Strict deployment drill: every storefront request must complete as 200.
import http from 'k6/http';
import { fail } from 'k6';
import { BASE_URL, SCENARIO, TAG, RATE, tenant_slugs, host_for,
  rate_scenario, build_summary, fetch_build_stamp, expect_status } from './lib.js';

const SLUGS = tenant_slugs(12);
export const options = {
  // Two requests/iteration; stay below production's 20 requests/s per IP.
  scenarios: { rolling_restart: rate_scenario(Number(RATE || 5)) },
  thresholds: { expected_status: ['rate==1'], dropped_iterations: ['count==0'] },
};
export function setup() {
  const build = fetch_build_stamp();
  if (build.error) fail(`restart setup: ${build.error}`);
  console.log('restart-load-ready');
  return { build };
}
export default function () {
  const headers = { Host: host_for(SLUGS[(__VU + __ITER) % SLUGS.length]) };
  for (const path of ['/api/v1/public/tenant-context', '/api/v1/public/theme']) {
    const response = http.get(`${BASE_URL}${path}`, { headers, redirects: 0 });
    expect_status(response, 200);
  }
}
export function handleSummary(data) {
  return { [`benchmarks/${SCENARIO}/${TAG}.json`]: build_summary(data, { notes: ['strict restart drill: every request 200, no dropped iterations'] }) };
}
