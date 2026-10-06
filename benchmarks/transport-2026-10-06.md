# Transport tuning — TASK-021 step 7 (slice 11.2h)

**Date:** 2026-10-06 · **Host:** the 11.2a Windows lab host · **Build:** debug
(test binaries; byte sizes are build-independent — serialization output,
not timings)

Step 7's four levers — compression with a floor, keep-alive, payload
trimming, ETags on the storefront read path — and the numbers each decision
rests on. Every number below was measured in this slice; ratios come from
CPython's `gzip` over the exact bytes the server renders (a throwaway probe
test wrote `/openapi.json`'s body to disk and was deleted afterwards).

## 1. The floor is honest at this API's payload sizes

| Payload | Raw bytes | gzip bytes | Saved |
|---|---|---|---|
| `GET /openapi.json` (the merged 14-context contract, `to_json` compact) | 320,559 | 51,496 (level 6; 67,000 at level 1) | ~84 % |
| Representative 422 problem body (219 B incl. `trace_id`) | 219 | 181 | 38 B |

Below ~1 KiB the saving is tens of bytes — less than the compressor's CPU
and framing are worth on the hottest paths (every error body lives down
there). Above it the savings are in the kilobytes and grow with the
document. So: **`min_bytes = 1024`** (`[server.compression]`, default on,
`enabled = false` is the rollback). The boundary is `>=` and a wire test
pins which side 1023 vs 1024 falls on.

The committed `api/*.yaml` total 247 KB across 14 specs — the same order of
magnitude as the served JSON, so the floor applies to every realistic
contract-shaped payload, not just the one measured.

## 2. What encodes, what never does

Negotiation reuses actix's `AcceptEncoding::negotiate` (RFC q-values, and
at equal q the server ranking **brotli > zstd > gzip > deflate** — the
round-trip tests prove br and gzip decode to the exact handler bytes).
The stock `Compress` middleware was rejected for three measured gaps, each
with a wire test proving the difference:

- **No floor** — it encodes an 11-byte body. Ours passes anything under
  1024 B through byte-identical.
- **Encodes already-compressed types** — `application/zip`, audio, pdf,
  octet-streams. Ours allow-lists textual representations only
  (`text/*`, `*+json`, `*+xml`, `application/json|xml|javascript|yaml`,
  SVG): a 4 KiB PNG passes through untouched.
- **Double compression by header alone** — ours refuses any response
  already carrying a non-`identity` `Content-Encoding`, byte-identical,
  with the single framing intact.

A client that refuses identity and every offered encoding gets `406` with
`Vary: Accept-Encoding` without the handler running (mirrors the stock
middleware). Every encoded response carries `Vary: Accept-Encoding` (the
stock middleware only sets it on its 406 path). Unknown content types fail
closed (identity) — a new textual representation opts into the allow-list.

## 3. Keep-alive: 5 s → 60 s

actix's default is `KeepAlive::Timeout(5 s)` (verified in
`actix-http-3.13.3/src/keep_alive.rs`): every idle gap past 5 s costs the
next request a full TCP+TLS handshake. `server.keep_alive_secs = 60`
(`0` disables) sits at the ubiquitous load-balancer idle default, so the
server rarely holds a connection the balancer has already dropped. The one
number this host cannot supply is the production balancer's actual idle
timeout — the value is recorded as tunable configuration, not as a
measurement, and stays a guess until the LB in front names its number.

## 4. ETags: what is covered, what was already covered

- **New:** `ConditionalEtag` over every successful GET/HEAD without a
  validator — strong ETag (`"sha256[..128]"` over the identity bytes),
  `304` on weak-comparison match (`*`, lists, `W/`), empty bodies and
  streams pass through. This covers the storefront reads that had none:
  `GET /api/v1/public/plans`, the four privacy disclosure reads, the
  sub-processor list — all slow-moving, all previously rebuilt *and*
  re-sent on every poll.
- **Already covered (left untouched):** `GET /api/v1/public/theme` and
  `GET /api/v1/public/messages/{domain}` manage their own validators; the
  middleware respects a present `ETag`.
- **New:** `/openapi.json` is pre-rendered once (`OnceLock`) with its ETag
  and `Cache-Control: public, no-cache` — the 11.2c profile's 25 ms
  rebuild disappears for both 200s (no rebuild, send bytes) and revalidations
  (headers only). Two reads are byte-identical, asserted by a test.
- **Deliberately not cached:** `/metrics` is live per scrape — it gets a
  validator like every other GET (harmless; no client sends
  `If-None-Match` for it) but no `Cache-Control`, so nothing may store it.

## 5. Payload trimming: audited, not cut

`to_json()` is already compact (`serde_json::to_string` — verified in
`utoipa-5.5.0/src/openapi.rs`), so there is no whitespace to trim. Field
removal would change response bodies, which this task explicitly forbids
("If a response body changes, it belongs in another task"), so the slice
audits and defers:

- `PublicPlanView` is already minimal by design (Stripe ids omitted).
- `PrivacyNoticeView` rows carry only rendered fields.
- `ResolvedTheme` carries both `tokens` and the derived `css_vars`, plus
  an in-body `etag` duplicating the header — all three are frontend-contract
  decisions (first paint needs `css_vars`, client swap needs `tokens`).
  Removing any of them belongs to the contract freeze (TASK-026) with the
  frontend's readership evidence, or frontend TASK-022 — recorded here so
  the next slice does not re-audit.

## 6. Behaviour preservation

No response body changes: compression and validators are header additions
plus byte-identical passthroughs below the floor. `scripts/check-openapi.sh`
is clean over all 14 specs; the route-matrix diff is fingerprint rows only
(`apps/api/src/lib.rs`, `sanvi-http` ×5 including the two new modules), no
route rows. Existing clients send no `Accept-Encoding` (test clients
included) and get bit-identical responses; the headers snapshot is green.
