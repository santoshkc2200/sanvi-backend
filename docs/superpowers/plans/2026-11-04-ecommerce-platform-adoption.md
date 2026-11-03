# Ecommerce Platform Adoption Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `sanvi-backend` consume the `ecommerce-service` quote, shipping, discount, fulfillment, return and payment contracts so shopper checkout and staff flows work end to end, not only with locally minted tokens.

**Architecture:** All work stays in `crates/contexts/product_access` (shopper orchestration, proxy, OpenAPI mirror) and `crates/contexts/access` (permission seeds). The shopper checkout stops pricing from scanned catalog data: it asks the service for a quote, signs the quote's fingerprint and totals into the checkout claims, and places the order with `expected_pricing_fingerprint`. Staff routes need no proxy change (the `{tail}` forward carries them); they need permission grants and a contract mirror.

**Tech Stack:** Rust, actix-web, SQLx, `scripts/test.sh`, generated OpenAPI under `api/`.

**Spec:** the service's own contracts, which are the source of truth for field names (read them, do not guess):
`ecommerce-service/docs/superpowers/plans/2026-11-01-checkout-quotes.md`, `…/2026-11-02-shipping-and-checkout-charges.md`, `…/2026-11-02-order-discounts.md` (each "Platform adoption follow-up"), `ecommerce-service/api/openapi.yaml` (`/orders/quote`, `/orders`, `/shipping-methods`), and `ecommerce-service/docs/architecture.md` ("Implemented: checkout quotes…", "shipping…", "order discounts").

## Global Constraints

- `CONTRIBUTING.md` is authoritative (layering, error mapping, OpenAPI, SQL). Do not run bare `cargo test`; use `scripts/test.sh -p product_access` / `-p access`, and `cargo c` / `cargo k` while iterating.
- Quote fingerprint is opaque: `v1:` (no shipping), `v2:` (shipping), `v3:` (discount). Never parse or recompute it; sign it as received and echo it back.
- Fingerprint, totals and shipping selection come only from signed checkout claims, never from browser input.
- `Cache-Control: no-store` must reach the browser on quote-derived and shipping-list responses.
- Staff-only permissions are never added to `WEB_CHECKOUT_PERMISSIONS` and never granted to `tenant_member`.
- Do not create a fresh sale or idempotency key after an ambiguous placement timeout; retry the original request.
- Out of scope: provider-payment gateway adapter and `sanvi.events.payments` stream (separate plan), storefront and admin UI (frontend repo), coupons, percentage discounts, carrier integration.

## Review Focus

- Old (pre-quote) checkout token presented at placement: must be refused with a refresh-required error, never placed as an unguarded order.
- Shipping method archived or edited between quote and placement: service 409 `ecommerce/checkout-price-changed` must surface as the shopper's re-approval state with the reservation still held.
- Ambiguous timeout on placement: the retry reuses the same `web-order-{key}` key and returns the original order.
- Tampered checkout token (changed fingerprint/totals): signature check fails; no upstream call is made.
- Shopper token asking for an unarchived-only shipping list must not gain `ecommerce.store.manage`.

---

### Task 1: Define and grant the remaining staff permissions

**Files:**
- Modify: `crates/contexts/access/src/domain/permissions.rs` (after `ecommerce.customer.privacy`, ~line 594)
- Modify: `crates/contexts/access/src/domain/roles.rs` (owner ~256-271, admin ~308-323 seeds, and the role tests ~476-519)
- Create: one forward migration only if existing role-grant seeds are migration-backed (check how `0408a32` shipped the phase-15 grants and mirror it exactly)

**Interfaces:**
- Produces: permission keys `ecommerce.order.discount`, `ecommerce.fulfillment.record`, `ecommerce.return.record`, `ecommerce.payment.record`, `ecommerce.payment.refund`, granted to owner and admin only.

- [ ] **Step 1: Write the failing test** in `roles.rs` tests: `staff_roles_hold_discount_fulfillment_return_and_payment_permissions` asserts owner and admin contain all five keys; `tenant_member_holds_none_of_the_ecommerce_write_permissions` asserts the member seed contains none of them.
- [ ] **Step 2: Run** `scripts/test.sh -p access --quiet`. Expected: FAIL (keys undefined).
- [ ] **Step 3: Add the five definitions** in `permissions.rs` copying the shape of `ecommerce.order.manage`, then add them to the owner and admin seeds with a one-line comment naming this plan.
- [ ] **Step 4: Run** `scripts/test.sh -p access --quiet`. Expected: PASS. Also run `just ci-quiet`'s permission/route-matrix generators (`scripts/generate-route-matrix.sh`) if they reference the seed list.
- [ ] **Step 5: Commit** `feat(access): define and grant ecommerce discount, fulfillment, return and payment permissions`.

### Task 2: Forward `Cache-Control` through the proxy

**Files:**
- Modify: `crates/contexts/product_access/src/domain/forwarding.rs:28` (`RESPONSE_HEADERS`)
- Test: same file's tests, plus `crates/contexts/product_access/tests/integration/proxy_http.rs`

**Interfaces:**
- Produces: `returns_response_header("cache-control") == true`; `proxy_http.rs` asserts an upstream `Cache-Control: no-store` reaches the client.

- [ ] **Step 1: Write failing tests**: `response_allow_list_includes_cache_control` (unit) and `proxy_returns_no_store_from_upstream` (integration, upstream stub sets the header).
- [ ] **Step 2: Run** `scripts/test.sh -p product_access --quiet`. Expected: FAIL.
- [ ] **Step 3: Add `"cache-control"`** to `RESPONSE_HEADERS`. Check `crates/platform/http/src/ssr_cache.rs` mentions ecommerce: confirm the shared cache layer does not override a forwarded `no-store`.
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit** `feat(product_access): forward cache-control from product services`.

### Task 3: Signed checkout claims carry the quote

**Files:**
- Modify: `crates/contexts/product_access/src/domain/shopper.rs` (`CheckoutClaims` ~300-344, `from_verified` ~348, tests ~447)

**Interfaces:**
- Produces: `CheckoutClaims` gains `pricing_fingerprint: String`, `shipping_method_id: Option<String>`, `subtotal_minor: i64`, `shipping_minor: i64`, `tax_minor: i64`, `total_minor: i64`; `CheckoutClaims::new(...)` takes a `QuotedTerms` struct (same fields) instead of four loose args; `from_verified` returns `ShopperError::CheckoutRefreshRequired` when `pricing_fingerprint` is missing (a legacy token).
- Consumes: nothing from other tasks.

- [ ] **Step 1: Write failing tests**: `claims_round_trip_with_quoted_terms`; `legacy_claims_without_fingerprint_require_refresh` (JSON from the old shape → `CheckoutRefreshRequired`); `wrong_audience_still_invalid`.
- [ ] **Step 2: Run** `scripts/test.sh -p product_access --quiet`. Expected: FAIL.
- [ ] **Step 3: Implement** the fields with `#[serde(default)]` only on `shipping_method_id`; add `ShopperError::CheckoutRefreshRequired` mapped to 409 `checkout-refresh-required` in the HTTP error mapper (`adapters/http/shopper_handlers.rs`) and the OpenAPI error list.
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit** `feat(product_access): bind quoted terms into signed checkout claims`.

### Task 4: `start_checkout` requests the quote

**Files:**
- Modify: `crates/contexts/product_access/src/application/shopper.rs` (`start_checkout` 627-787, doc structs ~1785-1975, `ShopperCheckoutView` 116, `ShopperStartCheckoutRequest` 250)
- Modify: `crates/contexts/product_access/src/adapters/http/shopper_handlers.rs` (set `Cache-Control: no-store` on the response)

**Interfaces:**
- Consumes: Task 3 `QuotedTerms`, `CheckoutClaims::new`.
- Produces: `ShopperStartCheckoutRequest.shipping_method_id: Option<String>`; `ShopperCheckoutView` gains `pricing_fingerprint` is **not** exposed raw beyond the signed token: it gains `shipping_minor`, `tax_minor`, `total_minor`, `shipping_method_name: Option<String>`; private `async fn quote(&self, tenant, store, reservation_id, customer_id, address_id, shipping_method_id: Option<&str>) -> Result<QuotedTerms, ShopperError>`.

- [ ] **Step 1: Write failing tests** in the `application/shopper.rs` test module using the existing upstream stub (`calls_to`): `checkout_requests_quote_after_hold_and_signs_its_terms`; `checkout_with_shipping_method_requires_address_and_quotes_v2`; `quote_failure_releases_hold_best_effort` (stub quote → 422; assert a release call and the original error returned).
- [ ] **Step 2: Run** `scripts/test.sh -p product_access --quiet`. Expected: FAIL.
- [ ] **Step 3: Implement** `quote` (POST `stores/{id}/orders/quote`, body per the service OpenAPI) called after the reservation hold; remove the catalog-derived `subtotal_minor` math and the `unit_price_minor` source for approved money (catalog scan stays for availability only). On any error after the hold, call the existing release path best-effort.
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit** `feat(product_access): price shopper checkout from the service quote`.

### Task 5: Guarded placement replaces compensating cancel

**Files:**
- Modify: `crates/contexts/product_access/src/application/shopper.rs` (`place_order` 886-937; delete `cancel_fresh_order` 939-1003)

**Interfaces:**
- Consumes: Task 3 claims, Task 4 quote.
- Produces: `place_order` body includes `expected_pricing_fingerprint` and `shipping_method_id` from claims; upstream 409 `ecommerce/checkout-price-changed` maps to the existing `ShopperError::PriceChanged`.

- [ ] **Step 1: Write failing tests**: `placement_sends_fingerprint_and_method_from_claims`; `price_changed_409_maps_to_price_changed_without_cancel_call` (assert zero calls to `/cancel`); `timeout_retry_reuses_the_same_idempotency_key`; `legacy_token_is_refused_before_any_upstream_call`.
- [ ] **Step 2: Run** `scripts/test.sh -p product_access --quiet`. Expected: FAIL.
- [ ] **Step 3: Implement**; remove the post-placement unit-price comparison and `cancel_fresh_order`, plus tests that exercised them.
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit** `feat(product_access): place shopper orders under the approved fingerprint`.

### Task 6: Shopper shipping-method listing

**Files:**
- Modify: `crates/contexts/product_access/src/application/shopper.rs` (new `pub async fn shipping_methods(&self, host: &str) -> Result<Vec<ShopperShippingMethodView>, ShopperError>`), `adapters/http/routes.rs`, `adapters/http/shopper_handlers.rs`

**Interfaces:**
- Produces: `GET /api/v1/public/ecommerce/shipping-methods` (follow the existing public shopper route prefix in `routes.rs`), returns only active methods with `id`, `name`, `amount_minor`, `tax_class`, always `Cache-Control: no-store`. It calls the service with the web-checkout permission set (no `store.manage`).

- [ ] **Step 1: Write failing tests**: `lists_only_active_methods_under_checkout_permissions` (assert upstream query `status=active`), `shipping_list_is_no_store`, and `archived_filter_cannot_be_requested_by_shopper` (client-supplied `status` ignored).
- [ ] **Step 2: Run** `scripts/test.sh -p product_access --quiet`. Expected: FAIL.
- [ ] **Step 3: Implement** the method, route and handler.
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit** `feat(product_access): expose active shipping methods to checkout`.

### Task 7: Mirror the contract and regenerate artifacts

**Files:**
- Modify: `crates/contexts/product_access/ecommerce-contract.yaml` (add quote, shipping-methods, fulfillments, returns, payments/refunds, discount object, new order fields and 409 slugs; follow the `/api/v1/tenant/ecommerce/...` pattern at line 1464+)
- Regenerate: `api/product_access.yaml`, `docs/api/generated/product_access.md`, `docs/security/route-matrix.md`

- [ ] **Step 1: Run** `scripts/check-openapi.sh`. Expected: FAIL after Tasks 3-6 until artifacts are regenerated.
- [ ] **Step 2: Extend the mirror** from `ecommerce-service/api/openapi.yaml`; keep staff permission names in each operation's 403 text.
- [ ] **Step 3: Regenerate** with `scripts/api-docs.sh` and `scripts/generate-route-matrix.sh`; update `apps/api/tests/integration/idor_sweep.rs` for any new tenant-scoped route.
- [ ] **Step 4: Run** `scripts/check-openapi.sh` and `scripts/test.sh -p product_access --quiet`. Expected: PASS.
- [ ] **Step 5: Commit** `docs(product_access): mirror quote, shipping, discount and fulfillment contracts`.

### Task 8: Gate, docs and handoff close-out

**Files:**
- Modify: `docs/roadmap.md` (phase 15 note), `CHANGELOG.md`, and add an "Adopted" note in `ecommerce-service/docs/architecture.md` is **not** done here (other repo)

- [ ] **Step 1: Run** `just ci-quiet` (or its `scripts/*.sh` equivalents). Expected: all gates pass; paste any failure's decisive line and fix at the owning task.
- [ ] **Step 2: Update** `docs/roadmap.md` and `CHANGELOG.md` with what is adopted and the remaining gap (provider payments, frontend).
- [ ] **Step 3: Commit** `docs: record ecommerce quote, shipping and discount adoption`.

---

## Self-review notes

- Coverage: quote adoption (Tasks 3-5), no-store (2), shipping selection/listing (4, 6), discount permission (1), fulfillment/return/payment permissions (1), mirror (7), legacy refresh (3, 5). Provider payments and UI are explicitly out of scope.
- Unverified before execution: the exact quote request/response field names, the discount object shape, and whether role grants are migration-backed. Each task points the implementer to the source to read; none is guessed here.
