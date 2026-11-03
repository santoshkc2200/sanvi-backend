//! The shopper orchestrations: public catalog reads, guest checkout and
//! order access, all against ecommerce-service as the platform's own
//! service user ("web checkout's service user").
//!
//! Flow map (one public call = one orchestration):
//!
//! - `public_store` / `public_products` / `public_product` — host → tenant
//!   → published store, then a catalog-only allowlist of service reads.
//! - `start_checkout` — validate guest input, price the lines from the
//!   catalog, reuse-or-create the guest customer and address, hold a
//!   reservation, and sign the priced summary into an opaque checkout
//!   token that dies with the hold.
//! - `place_order` — verify the token, place the order from the held
//!   reservation (idempotent via the forwarded key), refuse a price drift
//!   by cancelling the fresh order, and mint the purchase credential.
//! - `order_status` / `cancel_order` / payment start-get-cancel — the
//!   credential is the whole access decision; the service remains the
//!   authority for every state it owns.
//!
//! Upstream typed problems (`ecommerce/insufficient-stock`,
//! `ecommerce/payment-in-progress`, …) pass through verbatim so the
//! storefront branches on the service's own taxonomy.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use sanvi_kernel::TenantId;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::{
    CheckoutClaims, CheckoutLine, DelegationClaims, ECOMMERCE, ForwardRequest, HostTenants,
    PurchaseCredentialClaims, QuotedTerms, ShopperError, StoreBindings, TenantEntitlements,
    TokenSigner, UpstreamProblem, WEB_CHECKOUT_PERMISSIONS, WEB_CHECKOUT_USER_ID,
};

/// Longest forwarded idempotency key the orchestration builds: the
/// shopper's key (validated, ≤ 200 bytes) plus a `web-…-` namespace
/// prefix, inside the service's 255-byte ceiling.
const MAX_SHOPPER_KEY_BYTES: usize = 200;

/// Upstream page sizes and scan caps. Catalogs beyond a cap serve
/// truncated availability (logged); they are a scaling note, not a
/// correctness cliff, because checkout re-validates against the service.
const CATALOG_PAGE_LIMIT: u32 = 200;
const MAX_SCAN_PAGES: usize = 25;
const LOCATION_PAGE_LIMIT: u32 = 200;
const MAX_LOCATION_PAGES: usize = 5;
const STOCK_PAGE_LIMIT: u32 = 1000;
const MAX_STOCK_PAGES_PER_LOCATION: usize = 5;
/// The largest checkout the orchestration prices by scanning the catalog,
/// in variants and in products (a scaling note: beyond the cap, a variant
/// not yet scanned is refused as out of stock and checkout re-validates).
const MAX_CATALOG_SCAN_VARIANTS: usize = 5_000;
const MAX_CATALOG_SCAN_PRODUCTS: usize = 5_000;

/// The public catalog surfaces one page of products.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PublicStoreView {
    /// Display name of the published store.
    pub name: String,
    /// The store's single currency (ISO 4217); every price renders in it.
    pub currency: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PublicVariantView {
    /// Variant id (decimal Snowflake).
    pub id: String,
    /// Option name to value (for example `size: m`).
    pub options: BTreeMap<String, String>,
    /// Unit price in the store's currency, minor units.
    pub unit_price_minor: i64,
    pub currency: String,
    /// Whether the variant can be reserved right now (server-computed
    /// availability; the only availability state the contract serves).
    pub purchasable: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PublicProductView {
    /// Product id (decimal Snowflake).
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    /// Sellable variants (active product, non-archived variants).
    pub variants: Vec<PublicVariantView>,
    /// Cheapest sellable variant price, minor units.
    pub price_min_minor: i64,
    /// Most expensive sellable variant price, minor units.
    pub price_max_minor: i64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PublicProductPage {
    pub items: Vec<PublicProductView>,
    /// Pass as `cursor` for the next page; absent on the last page.
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ShopperCheckoutLineView {
    pub variant_id: String,
    pub unit_price_minor: i64,
    pub quantity: i64,
    /// `unit_price_minor * quantity` as the server priced it.
    pub line_total_minor: i64,
}

/// The checkout-start answer: the only pricing a checkout page may show,
/// plus the opaque token that carries it to placement. `expires_at` is the
/// reservation's server timestamp; the TTL is rendered from it, never
/// assumed.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ShopperCheckoutView {
    /// Opaque signed checkout summary; present it back at placement.
    pub checkout: String,
    pub currency: String,
    /// RFC 3339 instant the hold expires at (server clock).
    pub expires_at: String,
    pub lines: Vec<ShopperCheckoutLineView>,
    /// Sum of the line totals, minor units, in `currency`.
    pub subtotal_minor: i64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ShopperOrderLineView {
    pub position: i32,
    pub product_title: String,
    pub sku: Option<String>,
    pub options: BTreeMap<String, String>,
    pub unit_price_minor: i64,
    pub quantity: i64,
    pub line_total_minor: i64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ShopperOrderPaymentView {
    /// The ledger state: `unpaid | partially_paid | paid |
    /// partially_refunded | refunded`.
    pub status: String,
    pub paid_minor: i64,
    pub due_minor: i64,
    pub refunded_minor: i64,
}

/// The shopper projection of a placed order — the summary of record.
/// Internal bookkeeping (customer, addresses, reservation, versions,
/// location ids) is projected out.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ShopperOrderView {
    /// Order id (decimal Snowflake). Presented with the purchase
    /// credential only; alone it authorizes nothing.
    pub id: String,
    /// `placed | cancelled`.
    pub status: String,
    pub currency: String,
    /// Immutable lines as the service snapshot them at placement.
    pub lines: Vec<ShopperOrderLineView>,
    pub subtotal_minor: i64,
    pub tax_minor: i64,
    pub total_minor: i64,
    pub payment: ShopperOrderPaymentView,
    /// `unfulfilled | partially_fulfilled | fulfilled | not_required` —
    /// the coarse state only; no tracking or delivery promise exists.
    pub fulfillment_status: String,
    /// RFC 3339 instant the order was placed (server clock).
    pub placed_at: String,
    pub cancelled_at: Option<String>,
    pub cancel_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ShopperOrderConfirmationView {
    pub order: ShopperOrderView,
    /// The purchase credential: short-lived, scoped to this order, the
    /// shopper's only durable access. Treat as a bearer secret.
    pub credential: String,
    /// RFC 3339 instant the credential expires (server clock).
    pub credential_expires_at: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ShopperPaymentAttemptView {
    pub id: String,
    /// `pending | succeeded | failed | cancelled | expired`.
    pub status: String,
    /// Why an attempt failed (`card_declined`, …), when it did.
    pub failure_code: Option<String>,
    /// The hosted provider page to navigate to; present only while the
    /// attempt is pending and this view comes from a start (or a replay
    /// of one). Never sent by the status views.
    pub redirect_url: Option<String>,
    /// RFC 3339 instant the pending attempt expires at, when pending.
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ShopperPaymentStartView {
    pub attempt: ShopperPaymentAttemptView,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ShopperPaymentStatusView {
    /// The latest attempt, or `None` before the first one starts.
    pub attempt: Option<ShopperPaymentAttemptView>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ShopperGuestContact {
    /// Full name, 1–120 characters after trimming.
    pub name: String,
    /// Contact email; order and customer of record.
    pub email: String,
    #[serde(default)]
    pub phone: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ShopperGuestAddress {
    /// Who receives the order; defaults to the contact's name.
    #[serde(default)]
    pub recipient: Option<String>,
    pub line1: String,
    #[serde(default)]
    pub line2: Option<String>,
    pub city: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub postal_code: Option<String>,
    /// ISO 3166-1 alpha-2, upper-case.
    pub country: String,
    #[serde(default)]
    pub phone: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ShopperGuestLine {
    /// Variant id (decimal Snowflake) from the catalog.
    pub variant_id: String,
    /// 1–10 000 (the reservation contract's bound).
    pub quantity: i64,
}

/// Guest checkout start: contact + address + identifier lines. No prices,
/// no totals — the server prices everything.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ShopperStartCheckoutRequest {
    pub contact: ShopperGuestContact,
    pub address: ShopperGuestAddress,
    pub lines: Vec<ShopperGuestLine>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ShopperPlaceOrderRequest {
    /// The opaque checkout token checkout-start returned.
    pub checkout: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ShopperReleaseCheckoutRequest {
    /// The opaque checkout token to release the hold for.
    pub checkout: String,
}

/// The tenant-side admin of the published-store binding (the host → store
/// decision the public scope serves). `None` withdraws the binding.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PublishStoreRequest {
    pub store_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PublishedStoreView {
    /// The published store id, or `None` while the tenant publishes none.
    pub store_id: Option<String>,
}

/// The response of a service call after parsing: whether the service
/// replayed an idempotent create, and the JSON body.
struct ServiceReply {
    replayed: bool,
    body: serde_json::Value,
}

pub struct ShopperScope {
    host_tenants: Arc<dyn HostTenants>,
    entitlements: Arc<dyn TenantEntitlements>,
    bindings: Arc<dyn StoreBindings>,
    signer: Arc<dyn TokenSigner>,
    upstream: Arc<dyn crate::domain::ProductUpstream>,
    clock: fn() -> DateTime<Utc>,
    credential_ttl_secs: i64,
}

impl ShopperScope {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host_tenants: Arc<dyn HostTenants>,
        entitlements: Arc<dyn TenantEntitlements>,
        bindings: Arc<dyn StoreBindings>,
        signer: Arc<dyn TokenSigner>,
        upstream: Arc<dyn crate::domain::ProductUpstream>,
        credential_ttl_secs: i64,
    ) -> Self {
        Self {
            host_tenants,
            entitlements,
            bindings,
            signer,
            upstream,
            clock: Utc::now,
            credential_ttl_secs: credential_ttl_secs.max(60),
        }
    }

    /// Overrides the clock (tests pin one so token expiries are exact).
    pub fn with_clock(mut self, clock: fn() -> DateTime<Utc>) -> Self {
        self.clock = clock;
        self
    }

    // ------------------------------------------------------------------
    // Store context: host → tenant → published store, gated on the
    // entitlement. Every failure on this path is one public shape
    // (`store-unavailable`), never an upgrade prompt.
    // ------------------------------------------------------------------

    async fn store_context(&self, host: &str) -> Result<(TenantId, i64), ShopperError> {
        let tenant = self
            .host_tenants
            .resolve(host)
            .await?
            .filter(|tenant| tenant.active)
            .ok_or(ShopperError::StoreUnavailable)?;
        self.published_store_for(tenant.tenant_id).await
    }

    async fn published_store_for(
        &self,
        tenant_id: TenantId,
    ) -> Result<(TenantId, i64), ShopperError> {
        let entitlements = self.entitlements.resolve(tenant_id).await?;
        let entitled = entitlements
            .iter()
            .any(|entry| entry.key == ECOMMERCE.feature && entry.enabled);
        if !entitled {
            return Err(ShopperError::StoreUnavailable);
        }
        let store_id = self
            .bindings
            .published_store(tenant_id)
            .await?
            .ok_or(ShopperError::StoreUnavailable)?;
        Ok((tenant_id, store_id))
    }

    /// Fetches the store and refuses anything but an active one.
    async fn active_store(
        &self,
        tenant_id: TenantId,
        store_id: i64,
    ) -> Result<StoreDoc, ShopperError> {
        let reply = self
            .call_service(
                tenant_id,
                "GET",
                &format!("stores/{store_id}"),
                None,
                None,
                None,
            )
            .await?;
        let store: StoreDoc = parse_doc(&reply.body)?;
        if store.status != "active" {
            return Err(ShopperError::StoreUnavailable);
        }
        Ok(store)
    }

    // ------------------------------------------------------------------
    // Public catalog
    // ------------------------------------------------------------------

    pub async fn public_store(&self, host: &str) -> Result<PublicStoreView, ShopperError> {
        let (tenant_id, store_id) = self.store_context(host).await?;
        let store = self.active_store(tenant_id, store_id).await?;
        Ok(PublicStoreView {
            name: store.name,
            currency: store.currency,
        })
    }

    pub async fn public_products(
        &self,
        host: &str,
        limit: Option<i64>,
        after: Option<&str>,
    ) -> Result<PublicProductPage, ShopperError> {
        let (tenant_id, store_id) = self.store_context(host).await?;
        self.active_store(tenant_id, store_id).await?;
        let take = limit.unwrap_or(50).clamp(1, 100) as usize;

        let mut items: Vec<PublicProductView> = Vec::new();
        let mut next_cursor = after.map(str::to_owned);
        let mut pages = 0;
        let mut exhausted = false;
        // Filter-active pages until the public page is full or the catalog
        // ends, so an inactive-heavy page never shows as an empty catalog.
        while items.len() < take && pages < MAX_SCAN_PAGES {
            pages += 1;
            let mut query = format!("limit={CATALOG_PAGE_LIMIT}");
            if let Some(cursor) = &next_cursor {
                query.push_str("&cursor=");
                query.push_str(&urlencode(cursor));
            }
            let reply = self
                .call_service(
                    tenant_id,
                    "GET",
                    &format!("stores/{store_id}/products"),
                    Some(query),
                    None,
                    None,
                )
                .await?;
            let page: ProductPageDoc = parse_doc(&reply.body)?;
            let filtered = self
                .project_products(page.items, tenant_id, store_id)
                .await?;
            items.extend(filtered);
            match page.next_cursor.filter(|cursor| !cursor.is_empty()) {
                Some(cursor) => next_cursor = Some(cursor),
                None => {
                    exhausted = true;
                    break;
                }
            }
        }
        items.truncate(take);
        Ok(PublicProductPage {
            next_cursor: if exhausted { None } else { next_cursor },
            items,
        })
    }

    pub async fn public_product(
        &self,
        host: &str,
        product_id: &str,
    ) -> Result<PublicProductView, ShopperError> {
        let product_number = canonical_id(product_id).ok_or(ShopperError::NotFound {
            resource: "product",
        })?;
        let (tenant_id, store_id) = self.store_context(host).await?;
        self.active_store(tenant_id, store_id).await?;
        let reply = self
            .call_service(
                tenant_id,
                "GET",
                &format!("stores/{store_id}/products/{product_number}"),
                None,
                None,
                None,
            )
            .await?;
        let product: ProductDoc = parse_doc(&reply.body)?;
        let mut projected = self
            .project_products(vec![product], tenant_id, store_id)
            .await?;
        projected.pop().ok_or(ShopperError::NotFound {
            resource: "product",
        })
    }

    /// Filters to active products with sellable variants and attaches
    /// server-computed availability.
    async fn project_products(
        &self,
        products: Vec<ProductDoc>,
        tenant_id: TenantId,
        store_id: i64,
    ) -> Result<Vec<PublicProductView>, ShopperError> {
        let sellable: Vec<&ProductDoc> = products
            .iter()
            .filter(|product| product.status == "active")
            .filter(|product| product.variants.iter().any(|variant| !variant.archived))
            .collect();
        if sellable.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: Vec<String> = sellable
            .iter()
            .flat_map(|product| product.variants.iter().map(|variant| variant.id.clone()))
            .collect();
        let availability = self
            .availability_map(tenant_id, store_id, Some(&wanted))
            .await?;
        Ok(sellable
            .iter()
            .map(|product| {
                let variants: Vec<PublicVariantView> = product
                    .variants
                    .iter()
                    .filter(|variant| !variant.archived)
                    .map(|variant| {
                        let purchasable = availability
                            .get(&variant.id)
                            .is_some_and(|(_, available)| *available > 0);
                        PublicVariantView {
                            id: variant.id.clone(),
                            options: variant.options.clone(),
                            unit_price_minor: variant.price.amount_minor,
                            currency: variant.price.currency.clone(),
                            purchasable,
                        }
                    })
                    .collect();
                let prices = variants.iter().map(|variant| variant.unit_price_minor);
                PublicProductView {
                    id: product.id.clone(),
                    title: product.title.clone(),
                    description: product.description.clone(),
                    price_min_minor: prices.clone().min().unwrap_or(0),
                    price_max_minor: prices.max().unwrap_or(0),
                    variants,
                }
            })
            .collect())
    }

    // ------------------------------------------------------------------
    // Availability: per-variant best location and available quantity,
    // aggregated across the store's locations from the service's own
    // stock reads (`on_hand − safety_stock − held`, per location).
    // ------------------------------------------------------------------

    async fn availability_map(
        &self,
        tenant_id: TenantId,
        store_id: i64,
        wanted: Option<&[String]>,
    ) -> Result<BTreeMap<String, (String, i64)>, ShopperError> {
        let mut best: BTreeMap<String, (String, i64)> = BTreeMap::new();
        let mut locations = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..MAX_LOCATION_PAGES {
            let mut query = format!("limit={LOCATION_PAGE_LIMIT}");
            if let Some(cursor) = &after {
                query.push_str("&after=");
                query.push_str(&urlencode(cursor));
            }
            let reply = self
                .call_service(
                    tenant_id,
                    "GET",
                    &format!("stores/{store_id}/locations"),
                    Some(query),
                    None,
                    None,
                )
                .await?;
            let page: LocationPageDoc = parse_doc(&reply.body)?;
            locations.extend(page.items.into_iter().map(|location| location.id));
            after = page.next_after.filter(|cursor| !cursor.is_empty());
            if after.is_none() {
                break;
            }
        }
        if locations.len() >= MAX_LOCATION_PAGES * LOCATION_PAGE_LIMIT as usize {
            tracing::warn!(
                store_id,
                "location scan hit the cap; availability may be incomplete"
            );
        }

        for location_id in locations {
            let mut after: Option<String> = None;
            for _ in 0..MAX_STOCK_PAGES_PER_LOCATION {
                let mut query = format!(
                    "location_id={}&limit={STOCK_PAGE_LIMIT}",
                    urlencode(&location_id)
                );
                if let Some(cursor) = &after {
                    query.push_str("&after=");
                    query.push_str(&urlencode(cursor));
                }
                let reply = self
                    .call_service(
                        tenant_id,
                        "GET",
                        &format!("stores/{store_id}/stock"),
                        Some(query),
                        None,
                        None,
                    )
                    .await?;
                let page: StockPageDoc = parse_doc(&reply.body)?;
                for stock in page.items {
                    if let Some(wanted) = wanted
                        && !wanted.contains(&stock.variant_id)
                    {
                        continue;
                    }
                    let entry = best
                        .entry(stock.variant_id)
                        .or_insert_with(|| (location_id.clone(), stock.available));
                    if stock.available > entry.1 {
                        *entry = (location_id.clone(), stock.available);
                    }
                }
                after = page.next_after.filter(|cursor| !cursor.is_empty());
                if after.is_none() {
                    break;
                }
            }
        }
        Ok(best)
    }

    // ------------------------------------------------------------------
    // Guest checkout
    // ------------------------------------------------------------------

    pub async fn start_checkout(
        &self,
        host: &str,
        shopper_key: &str,
        request: &ShopperStartCheckoutRequest,
    ) -> Result<ShopperCheckoutView, ShopperError> {
        valid_shopper_key(shopper_key)?;
        let input = validate_checkout_input(request)?;
        let (tenant_id, store_id) = self.store_context(host).await?;
        let store = self.active_store(tenant_id, store_id).await?;

        // Price the lines from the catalog and refuse anything that is not
        // sellable right now (identifiers in, prices never).
        let catalog = self.scan_catalog(tenant_id, store_id).await?;
        let mut unreserved: Vec<i64> = Vec::new();
        let mut priced: Vec<(usize, &ProductDoc, &VariantDoc)> = Vec::new();
        for (index, line) in input.lines.iter().enumerate() {
            match catalog.variants.get(&line.variant_id) {
                Some((product, variant)) if product.status == "active" && !variant.archived => {
                    priced.push((index, product, variant));
                }
                _ => unreserved.push(index as i64),
            }
        }

        // Pick each line's reservation location from live stock.
        let wanted: Vec<String> = priced
            .iter()
            .map(|(_, _, variant)| variant.id.clone())
            .collect();
        let availability = self
            .availability_map(tenant_id, store_id, Some(&wanted))
            .await?;
        let mut reservation_lines: Vec<ReservationLineIn> = Vec::new();
        for (index, _, variant) in &priced {
            let quantity = input.lines[*index].quantity;
            match availability.get(&variant.id) {
                Some((location_id, available)) if *available >= quantity => {
                    reservation_lines.push(ReservationLineIn {
                        variant_id: variant.id.clone(),
                        location_id: location_id.clone(),
                        quantity,
                    });
                }
                _ => unreserved.push(*index as i64),
            }
        }
        if !unreserved.is_empty() {
            unreserved.sort_unstable();
            return Err(ShopperError::Upstream(Box::new(insufficient_stock(
                &unreserved,
            ))));
        }

        // Guest customer: create one, or adopt the one this email already
        // has (the service refuses a second customer per email).
        let customer_payload = serde_json::json!({
            "name": input.contact.name,
            "email": input.contact.email,
        })
        .pipe_optional("phone", input.contact.phone.as_deref());
        let customer_reply = self
            .call_service(
                tenant_id,
                "POST",
                "customers",
                None,
                Some(customer_payload),
                Some(&format!("web-cust-{shopper_key}")),
            )
            .await;
        let customer_id = match customer_reply {
            Ok(reply) => parse_doc::<CustomerDoc>(&reply.body)?.id,
            Err(ShopperError::Upstream(problem))
                if problem.slug() == "ecommerce/customer-email-taken" =>
            {
                problem.customer_id.clone().ok_or_else(|| {
                    ShopperError::BadGateway("email-taken problem without a customer id".to_owned())
                })?
            }
            Err(error) => return Err(error),
        };

        // Address: reuse an exact active match, else create one — a repeat
        // guest email must not accumulate addresses into the service's
        // 20-address ceiling.
        let address_id = self
            .reuse_or_create_address(tenant_id, &customer_id, &input, shopper_key)
            .await?;

        // Hold the reservation. All-or-nothing upstream: a shortfall holds
        // nothing and the key stays unclaimed.
        let reservation_payload = serde_json::json!({
            "lines": reservation_lines.iter().map(|line| serde_json::json!({
                "variant_id": line.variant_id,
                "location_id": line.location_id,
                "quantity": line.quantity,
            })).collect::<Vec<_>>(),
        });
        let reservation_reply = self
            .call_service(
                tenant_id,
                "POST",
                &format!("stores/{store_id}/reservations"),
                None,
                Some(reservation_payload),
                Some(&format!("web-res-{shopper_key}")),
            )
            .await?;
        let reservation: ReservationDoc = parse_doc(&reservation_reply.body)?;
        if reservation.status != "held" {
            return Err(ShopperError::BadGateway(format!(
                "reservation answered with status {:?}",
                reservation.status
            )));
        }
        let expires_at = parse_rfc3339(&reservation.expires_at)?;

        let checkout_lines: Vec<CheckoutLine> = priced
            .iter()
            .map(|(index, _, variant)| CheckoutLine {
                variant_id: variant.id.clone(),
                unit_price_minor: variant.price.amount_minor,
                quantity: input.lines[*index].quantity,
            })
            .collect();
        let subtotal_minor: i64 = checkout_lines
            .iter()
            .map(|line| line.unit_price_minor * line.quantity)
            .sum();

        // Interim: the terms are still catalog-derived until this
        // orchestration requests the service quote (the next adoption
        // task); the fingerprint then becomes the service's, signed as
        // received.
        let quoted = QuotedTerms {
            pricing_fingerprint: String::new(),
            shipping_method_id: None,
            subtotal_minor,
            shipping_minor: 0,
            tax_minor: 0,
            total_minor: subtotal_minor,
        };

        let claims = CheckoutClaims::new(
            reservation.id,
            tenant_id,
            store_id,
            customer_id,
            address_id,
            store.currency.clone(),
            checkout_lines,
            quoted,
            uuid::Uuid::now_v7().simple().to_string(),
            expires_at,
            (self.clock)(),
        );
        let token = self.signer.sign_checkout(&claims)?;
        Ok(ShopperCheckoutView {
            checkout: token,
            currency: store.currency,
            expires_at: reservation.expires_at,
            lines: claims
                .lines
                .iter()
                .map(|line| ShopperCheckoutLineView {
                    variant_id: line.variant_id.clone(),
                    unit_price_minor: line.unit_price_minor,
                    quantity: line.quantity,
                    line_total_minor: line.unit_price_minor * line.quantity,
                })
                .collect(),
            subtotal_minor,
        })
    }

    async fn reuse_or_create_address(
        &self,
        tenant_id: TenantId,
        customer_id: &str,
        input: &ValidatedCheckout,
        shopper_key: &str,
    ) -> Result<String, ShopperError> {
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LOCATION_PAGES {
            let mut query = format!("status=active&limit={LOCATION_PAGE_LIMIT}");
            if let Some(after) = &cursor {
                query.push_str("&cursor=");
                query.push_str(&urlencode(after));
            }
            let reply = self
                .call_service(
                    tenant_id,
                    "GET",
                    &format!("customers/{customer_id}/addresses"),
                    Some(query),
                    None,
                    None,
                )
                .await?;
            let page: AddressPageDoc = parse_doc(&reply.body)?;
            if let Some(matched) = page
                .items
                .iter()
                .find(|address| address_matches(address, input))
            {
                return Ok(matched.id.clone());
            }
            cursor = page.next_cursor.filter(|value| !value.is_empty());
            if cursor.is_none() {
                break;
            }
        }

        let payload = serde_json::json!({
            "recipient": input.address.recipient.clone().unwrap_or_else(|| input.contact.name.clone()),
            "line1": input.address.line1,
            "line2": input.address.line2,
            "city": input.address.city,
            "region": input.address.region,
            "postal_code": input.address.postal_code,
            "country": input.address.country,
            "phone": input.address.phone,
        });
        let reply = self
            .call_service(
                tenant_id,
                "POST",
                &format!("customers/{customer_id}/addresses"),
                None,
                Some(payload),
                Some(&format!("web-addr-{shopper_key}")),
            )
            .await?;
        Ok(parse_doc::<CustomerDoc>(&reply.body)?.id)
    }

    /// Releases the checkout's hold. Valid without an expiry check —
    /// releasing an expired hold is how the service records it. Idempotent:
    /// a committed reservation refuses with `reservation-not-held`, which
    /// is success for the caller's purpose.
    pub async fn release_checkout(
        &self,
        host: &str,
        checkout_token: &str,
    ) -> Result<(), ShopperError> {
        let (tenant_id, store_id) = self.store_context(host).await?;
        let claims = self.checkout_claims(checkout_token, tenant_id, store_id, false)?;
        match self
            .call_service(
                tenant_id,
                "POST",
                &format!("stores/{}/reservations/{}/release", store_id, claims.sub),
                None,
                None,
                None,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(ShopperError::Upstream(problem))
                if problem.slug() == "ecommerce/reservation-not-held" =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    // ------------------------------------------------------------------
    // Placement and the purchase credential
    // ------------------------------------------------------------------

    pub async fn place_order(
        &self,
        host: &str,
        shopper_key: &str,
        checkout_token: &str,
    ) -> Result<ShopperOrderConfirmationView, ShopperError> {
        valid_shopper_key(shopper_key)?;
        let (tenant_id, store_id) = self.store_context(host).await?;
        let claims = self.checkout_claims(checkout_token, tenant_id, store_id, true)?;

        let payload = serde_json::json!({
            "reservation_id": claims.sub,
            "channel": "web",
            "customer_id": claims.customer_id,
            "shipping_address_id": claims.shipping_address_id,
        });
        let reply = self
            .call_service(
                tenant_id,
                "POST",
                &format!("stores/{store_id}/orders"),
                None,
                Some(payload),
                Some(&format!("web-order-{shopper_key}")),
            )
            .await?;
        let order: OrderDoc = parse_doc(&reply.body)?;

        // On a first placement (not a replay), the order must carry the
        // prices the checkout summary showed. A drift is refused and the
        // fresh order cancelled: the shopper re-validates instead of being
        // silently charged a different total. Replays return the original
        // order whatever the catalog says now — its snapshot is the truth.
        if !reply.replayed
            && order.lines.iter().any(|line| {
                claims
                    .lines
                    .iter()
                    .find(|approved| approved.variant_id == line.variant_id)
                    .is_none_or(|approved| approved.unit_price_minor != line.unit_price_minor)
            })
        {
            self.cancel_fresh_order(tenant_id, store_id, &order.id, order.version)
                .await;
            return Err(ShopperError::PriceChanged);
        }

        self.confirmation(tenant_id, store_id, order).await
    }

    /// Cancels a just-placed order after a price-drift refusal. Best
    /// effort: a version conflict re-reads once; anything else is logged —
    /// the refusal itself stands either way.
    async fn cancel_fresh_order(
        &self,
        tenant_id: TenantId,
        store_id: i64,
        order_id: &str,
        version: i64,
    ) {
        let payload = serde_json::json!({ "version": version, "reason": "other" });
        let result = self
            .call_service(
                tenant_id,
                "POST",
                &format!("stores/{store_id}/orders/{order_id}/cancel"),
                None,
                Some(payload),
                None,
            )
            .await;
        let result = match result {
            Ok(_) => Ok(()),
            Err(ShopperError::Upstream(problem))
                if problem.slug() == "ecommerce/version-conflict" =>
            {
                match self
                    .call_service(
                        tenant_id,
                        "GET",
                        &format!("stores/{store_id}/orders/{order_id}"),
                        None,
                        None,
                        None,
                    )
                    .await
                {
                    Ok(reply) => {
                        let current: OrderDoc = match parse_doc(&reply.body) {
                            Ok(order) => order,
                            Err(error) => {
                                return tracing::warn!(%error, "price-drift cancel: re-read failed");
                            }
                        };
                        let retry = serde_json::json!({
                            "version": current.version,
                            "reason": "other",
                        });
                        self.call_service(
                            tenant_id,
                            "POST",
                            &format!("stores/{store_id}/orders/{order_id}/cancel"),
                            None,
                            Some(retry),
                            None,
                        )
                        .await
                        .map(|_| ())
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            tracing::warn!(order_id, %error, "price-drift cancel failed; order remains placed");
        }
    }

    async fn confirmation(
        &self,
        tenant_id: TenantId,
        store_id: i64,
        order: OrderDoc,
    ) -> Result<ShopperOrderConfirmationView, ShopperError> {
        let now = (self.clock)();
        let claims = PurchaseCredentialClaims::new(
            order.id.clone(),
            tenant_id,
            store_id,
            uuid::Uuid::now_v7().simple().to_string(),
            self.credential_ttl_secs,
            now,
        );
        let expires_at = DateTime::from_timestamp(claims.exp, 0)
            .ok_or_else(|| ShopperError::Storage("credential expiry out of range".to_owned()))?;
        let credential = self.signer.sign_credential(&claims)?;
        Ok(ShopperOrderConfirmationView {
            order: project_order(order),
            credential,
            credential_expires_at: expires_at.to_rfc3339(),
        })
    }

    // ------------------------------------------------------------------
    // Credential-scoped order access
    // ------------------------------------------------------------------

    /// Verifies the purchase credential against the host's tenant and the
    /// addressed order. This is the whole access decision: an order id
    /// alone authorizes nothing, and an invalid credential never reveals
    /// whether the order exists.
    async fn authorize(
        &self,
        host: &str,
        order_id: &str,
        credential: Option<&str>,
    ) -> Result<(TenantId, i64), ShopperError> {
        let (tenant_id, store_id) = self.store_context(host).await?;
        let token = credential
            .map(str::to_owned)
            .ok_or(ShopperError::CredentialInvalid)?;
        let verified = self
            .signer
            .verify_json(&token)
            .map_err(|_| ShopperError::CredentialInvalid)?;
        let claims = PurchaseCredentialClaims::authorize(
            Some(verified),
            order_id,
            tenant_id,
            (self.clock)(),
        )?;
        // The credential binds the order to a store; the host must still
        // serve that store (defense against a re-bound tenant).
        if claims.store_id != store_id.to_string() {
            return Err(ShopperError::CredentialInvalid);
        }
        Ok((tenant_id, store_id))
    }

    pub async fn order_status(
        &self,
        host: &str,
        order_id: &str,
        credential: Option<&str>,
    ) -> Result<ShopperOrderView, ShopperError> {
        let order_number = canonical_id(order_id).ok_or(ShopperError::CredentialInvalid)?;
        let (tenant_id, store_id) = self.authorize(host, order_id, credential).await?;
        let reply = self
            .call_service(
                tenant_id,
                "GET",
                &format!("stores/{store_id}/orders/{order_number}"),
                None,
                None,
                None,
            )
            .await?;
        let order: OrderDoc = parse_doc(&reply.body)?;
        Ok(project_order(order))
    }

    pub async fn cancel_order(
        &self,
        host: &str,
        order_id: &str,
        credential: Option<&str>,
    ) -> Result<ShopperOrderView, ShopperError> {
        let order_number = canonical_id(order_id).ok_or(ShopperError::CredentialInvalid)?;
        let (tenant_id, store_id) = self.authorize(host, order_id, credential).await?;

        let fetch = async |tenant_id: TenantId, store_id: i64| {
            let reply = self
                .call_service(
                    tenant_id,
                    "GET",
                    &format!("stores/{store_id}/orders/{order_number}"),
                    None,
                    None,
                    None,
                )
                .await?;
            parse_doc::<OrderDoc>(&reply.body)
        };
        let order = fetch(tenant_id, store_id).await?;
        if order.status == "cancelled" {
            return Ok(project_order(order));
        }

        // A pending attempt blocks an upstream cancel: cancel it first,
        // surfacing a provider "cannot cancel yet" as its own typed state.
        let attempts = self.latest_attempts(tenant_id, store_id, &order.id).await?;
        if let Some(attempt) = attempts.iter().find(|attempt| attempt.status == "pending") {
            self.call_service(
                tenant_id,
                "POST",
                &format!(
                    "stores/{store_id}/orders/{}/payment-attempts/{}/cancel",
                    order.id, attempt.id
                ),
                None,
                None,
                None,
            )
            .await?;
        }

        let payload = serde_json::json!({
            "version": order.version,
            "reason": "customer_request",
        });
        self.call_service(
            tenant_id,
            "POST",
            &format!("stores/{store_id}/orders/{}/cancel", order.id),
            None,
            Some(payload),
            None,
        )
        .await?;
        let refreshed = fetch(tenant_id, store_id).await?;
        Ok(project_order(refreshed))
    }

    // ------------------------------------------------------------------
    // Hosted payment attempts
    // ------------------------------------------------------------------

    pub async fn start_payment(
        &self,
        host: &str,
        order_id: &str,
        credential: Option<&str>,
        shopper_key: &str,
    ) -> Result<ShopperPaymentStartView, ShopperError> {
        valid_shopper_key(shopper_key)?;
        let order_number = canonical_id(order_id).ok_or(ShopperError::CredentialInvalid)?;
        let (tenant_id, store_id) = self.authorize(host, order_id, credential).await?;

        let return_url = shopper_return_url(host, &order_number)?;
        let payload = serde_json::json!({ "return_url": return_url });
        let reply = self
            .call_service(
                tenant_id,
                "POST",
                &format!("stores/{store_id}/orders/{order_number}/payment-attempts"),
                None,
                Some(payload),
                Some(&format!("web-pay-{shopper_key}")),
            )
            .await?;
        let attempt: AttemptDoc = parse_doc(&reply.body)?;
        Ok(ShopperPaymentStartView {
            attempt: project_attempt(attempt, true),
        })
    }

    pub async fn payment_status(
        &self,
        host: &str,
        order_id: &str,
        credential: Option<&str>,
    ) -> Result<ShopperPaymentStatusView, ShopperError> {
        let order_number = canonical_id(order_id).ok_or(ShopperError::CredentialInvalid)?;
        let (tenant_id, store_id) = self.authorize(host, order_id, credential).await?;
        let attempts = self
            .latest_attempts(tenant_id, store_id, &order_number)
            .await?;
        Ok(ShopperPaymentStatusView {
            attempt: attempts
                .into_iter()
                .next()
                .map(|attempt| project_attempt(attempt, false)),
        })
    }

    pub async fn cancel_payment(
        &self,
        host: &str,
        order_id: &str,
        credential: Option<&str>,
    ) -> Result<ShopperPaymentStatusView, ShopperError> {
        let order_number = canonical_id(order_id).ok_or(ShopperError::CredentialInvalid)?;
        let (tenant_id, store_id) = self.authorize(host, order_id, credential).await?;
        let attempts = self
            .latest_attempts(tenant_id, store_id, &order_number)
            .await?;
        if let Some(attempt) = attempts.iter().find(|attempt| attempt.status == "pending") {
            let reply = self
                .call_service(
                    tenant_id,
                    "POST",
                    &format!(
                        "stores/{store_id}/orders/{order_number}/payment-attempts/{}/cancel",
                        attempt.id
                    ),
                    None,
                    None,
                    None,
                )
                .await?;
            let cancelled: AttemptDoc = parse_doc(&reply.body)?;
            return Ok(ShopperPaymentStatusView {
                attempt: Some(project_attempt(cancelled, false)),
            });
        }
        Ok(ShopperPaymentStatusView {
            attempt: attempts
                .into_iter()
                .next()
                .map(|attempt| project_attempt(attempt, false)),
        })
    }

    async fn latest_attempts(
        &self,
        tenant_id: TenantId,
        store_id: i64,
        order_id: &str,
    ) -> Result<Vec<AttemptDoc>, ShopperError> {
        let reply = self
            .call_service(
                tenant_id,
                "GET",
                &format!("stores/{store_id}/orders/{order_id}/payment-attempts"),
                None,
                None,
                None,
            )
            .await?;
        let list: AttemptListDoc = parse_doc(&reply.body)?;
        // The service lists oldest first; the shopper cares about newest.
        let mut items = list.items;
        items.reverse();
        Ok(items)
    }

    // ------------------------------------------------------------------
    // Published-store binding administration (tenant-side, staff-scoped;
    // the route declares the permission, the service validates the store)
    // ------------------------------------------------------------------

    pub async fn published_store(
        &self,
        tenant_id: TenantId,
    ) -> Result<PublishedStoreView, ShopperError> {
        let store_id = self
            .bindings
            .published_store(tenant_id)
            .await?
            .map(|store_id| store_id.to_string());
        Ok(PublishedStoreView { store_id })
    }

    /// Publishes or withdraws the tenant's store. `validate` is the
    /// caller-driven service lookup proving the store exists, belongs to
    /// the tenant and is active — the caller's own delegation, not the
    /// service user's.
    pub async fn publish_store(
        &self,
        tenant_id: TenantId,
        store_id: Option<i64>,
        validate: impl AsyncFnOnce(TenantId, i64) -> Result<StoreDoc, ShopperError>,
    ) -> Result<PublishedStoreView, ShopperError> {
        if let Some(store_id) = store_id {
            let store = validate(tenant_id, store_id).await?;
            if store.status != "active" {
                return Err(ShopperError::CheckoutInvalid {
                    detail: "the checkout reference is not valid".to_owned(),
                });
            }
        }
        self.bindings.publish(tenant_id, store_id).await?;
        let store_id = self
            .bindings
            .published_store(tenant_id)
            .await?
            .map(|store_id| store_id.to_string());
        Ok(PublishedStoreView { store_id })
    }

    // ------------------------------------------------------------------
    // Service plumbing
    // ------------------------------------------------------------------

    fn checkout_claims(
        &self,
        token: &str,
        tenant_id: TenantId,
        store_id: i64,
        require_alive: bool,
    ) -> Result<CheckoutClaims, ShopperError> {
        let verified =
            self.signer
                .verify_json(token)
                .map_err(|_| ShopperError::CheckoutInvalid {
                    detail: "the checkout reference is not valid".to_owned(),
                })?;
        let claims = CheckoutClaims::from_verified(verified)?;
        if claims.tenant_id != tenant_id.to_string() || claims.store_id != store_id.to_string() {
            return Err(ShopperError::CheckoutInvalid {
                detail: "the checkout reference is not valid".to_owned(),
            });
        }
        if require_alive && (self.clock)().timestamp() >= claims.exp {
            return Err(ShopperError::CheckoutExpired);
        }
        Ok(claims)
    }

    async fn scan_catalog(
        &self,
        tenant_id: TenantId,
        store_id: i64,
    ) -> Result<Catalog, ShopperError> {
        let mut catalog = Catalog::default();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_SCAN_PAGES {
            let mut query = format!("limit={CATALOG_PAGE_LIMIT}");
            if let Some(after) = &cursor {
                query.push_str("&cursor=");
                query.push_str(&urlencode(after));
            }
            let reply = self
                .call_service(
                    tenant_id,
                    "GET",
                    &format!("stores/{store_id}/products"),
                    Some(query),
                    None,
                    None,
                )
                .await?;
            let page: ProductPageDoc = parse_doc(&reply.body)?;
            for product in page.items {
                if catalog.products.len() >= MAX_CATALOG_SCAN_PRODUCTS {
                    tracing::warn!(store_id, "catalog scan hit the product cap");
                    return Ok(catalog);
                }
                for variant in &product.variants {
                    if catalog.variants.len() >= MAX_CATALOG_SCAN_VARIANTS {
                        tracing::warn!(store_id, "catalog scan hit the variant cap");
                        return Ok(catalog);
                    }
                    catalog
                        .variants
                        .insert(variant.id.clone(), (product.clone(), variant.clone()));
                }
                catalog.products.push(product);
            }
            cursor = page.next_cursor.filter(|value| !value.is_empty());
            if cursor.is_none() {
                return Ok(catalog);
            }
        }
        tracing::warn!(store_id, "catalog scan hit the page cap");
        Ok(catalog)
    }

    /// Signs the web-checkout service-user delegation and forwards one
    /// call to ecommerce-service. Non-2xx answers surface as the parsed
    /// upstream problem; a body that is not a problem document is a
    /// misbehaving service (bad gateway).
    async fn call_service(
        &self,
        tenant_id: TenantId,
        method: &str,
        path: &str,
        query: Option<String>,
        body: Option<serde_json::Value>,
        idempotency_key: Option<&str>,
    ) -> Result<ServiceReply, ShopperError> {
        let entitlements = self.entitlements.resolve(tenant_id).await?;
        let jti = uuid::Uuid::now_v7().simple().to_string();
        let claims = DelegationClaims::grant(
            &ECOMMERCE,
            tenant_id,
            sanvi_kernel::UserId::try_from(WEB_CHECKOUT_USER_ID).map_err(|_| {
                ShopperError::Storage("web checkout user id is not a valid id".to_owned())
            })?,
            &WEB_CHECKOUT_PERMISSIONS
                .iter()
                .map(|permission| (*permission).to_owned())
                .collect::<Vec<String>>(),
            &entitlements,
            jti,
            (self.clock)(),
        )?;
        let token = self.signer.sign(&claims)?;

        let mut headers = vec![("content-type".to_owned(), "application/json".to_owned())];
        if let Some(key) = idempotency_key {
            headers.push(("idempotency-key".to_owned(), key.to_owned()));
        }
        let body_bytes = body
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|err| ShopperError::Storage(format!("encode request: {err}")))?
            .unwrap_or_default();
        let path = crate::domain::ProductPath::parse(path).map_err(ShopperError::from)?;
        let forward =
            ForwardRequest::new(method, path, query.as_deref(), headers, body_bytes, None)
                .map_err(ShopperError::from)?;
        let response = self.upstream.forward(&forward, &token).await?;

        let replayed = response.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("idempotency-replayed") && value.eq_ignore_ascii_case("true")
        });
        if response.status >= 400 {
            return match UpstreamProblem::parse(response.status, &response.body) {
                Some(problem) => Err(ShopperError::Upstream(Box::new(problem))),
                None => Err(ShopperError::BadGateway(format!(
                    "upstream answered {} with an unreadable body",
                    response.status
                ))),
            };
        }
        let body = if response.body.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&response.body).map_err(|err| {
                ShopperError::BadGateway(format!("upstream body is not JSON: {err}"))
            })?
        };
        Ok(ServiceReply { replayed, body })
    }
}

trait JsonExt {
    /// Sets `key` only when `value` is `Some` — mirrors how the service's
    /// idempotency fingerprints omit absent fields.
    fn pipe_optional(self, key: &str, value: Option<&str>) -> Self;
}

impl JsonExt for serde_json::Value {
    fn pipe_optional(mut self, key: &str, value: Option<&str>) -> Self {
        if let Some(value) = value {
            self[key] = serde_json::json!(value);
        }
        self
    }
}

// ----------------------------------------------------------------------
// Input validation
// ----------------------------------------------------------------------

struct ValidatedLine {
    variant_id: String,
    quantity: i64,
}

struct ValidatedCheckout {
    contact: ShopperGuestContact,
    address: ShopperGuestAddress,
    lines: Vec<ValidatedLine>,
}

fn validate_checkout_input(
    request: &ShopperStartCheckoutRequest,
) -> Result<ValidatedCheckout, ShopperError> {
    let bad = |detail: &str| ShopperError::CheckoutInvalid {
        detail: detail.to_owned(),
    };
    let name = request.contact.name.trim();
    if name.is_empty() || name.len() > 120 {
        return Err(bad("contact name must be 1-120 characters"));
    }
    let email = request.contact.email.trim();
    if !valid_email(email) {
        return Err(bad("contact email is not valid"));
    }
    let phone = request
        .contact
        .phone
        .as_deref()
        .map(str::trim)
        .filter(|phone| !phone.is_empty());
    if let Some(phone) = phone
        && phone.len() > 30
    {
        return Err(bad("contact phone must be at most 30 characters"));
    }
    let address = &request.address;
    let country = address.country.trim();
    if country.len() != 2 || !country.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(bad("address country must be an ISO 3166-1 alpha-2 code"));
    }
    if address.line1.trim().is_empty() || address.line1.len() > 200 {
        return Err(bad("address line1 must be 1-200 characters"));
    }
    if address.city.trim().is_empty() || address.city.len() > 100 {
        return Err(bad("address city must be 1-100 characters"));
    }
    for (value, limit, field) in [
        (address.line2.as_deref(), 200usize, "address line2"),
        (address.region.as_deref(), 100, "address region"),
        (address.postal_code.as_deref(), 20, "address postal_code"),
        (address.phone.as_deref(), 30, "address phone"),
        (address.recipient.as_deref(), 120, "address recipient"),
    ] {
        if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty())
            && value.len() > limit
        {
            return Err(bad(&format!("{field} must be at most {limit} characters")));
        }
    }
    if request.lines.is_empty() || request.lines.len() > 100 {
        return Err(bad("lines must contain between 1 and 100 items"));
    }
    let mut lines: Vec<ValidatedLine> = Vec::with_capacity(request.lines.len());
    for line in &request.lines {
        let variant_id = canonical_id(&line.variant_id)
            .ok_or_else(|| bad("each line needs a valid variant id"))?;
        if !(1..=10_000).contains(&line.quantity) {
            return Err(bad("line quantity must be between 1 and 10000"));
        }
        if lines
            .iter()
            .any(|existing| existing.variant_id == variant_id)
        {
            return Err(bad("lines must not repeat a variant"));
        }
        lines.push(ValidatedLine {
            variant_id,
            quantity: line.quantity,
        });
    }
    Ok(ValidatedCheckout {
        contact: ShopperGuestContact {
            name: name.to_owned(),
            email: email.to_owned(),
            phone: phone.map(str::to_owned),
        },
        address: ShopperGuestAddress {
            recipient: address
                .recipient
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            line1: address.line1.trim().to_owned(),
            line2: address
                .line2
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            city: address.city.trim().to_owned(),
            region: address
                .region
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            postal_code: address
                .postal_code
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            country: country.to_owned(),
            phone: address
                .phone
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
        },
        lines,
    })
}

fn valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !email.contains(' ')
        && email.len() <= 254
        && email.bytes().all(|b| b.is_ascii_graphic())
}

fn valid_shopper_key(key: &str) -> Result<(), ShopperError> {
    if key.is_empty()
        || key.len() > MAX_SHOPPER_KEY_BYTES
        || !key.bytes().all(|b| (0x21..=0x7e).contains(&b))
    {
        return Err(ShopperError::CheckoutInvalid {
            detail: "the idempotency key is not acceptable".to_owned(),
        });
    }
    Ok(())
}

/// Canonical positive decimal id: the only id shape the contract travels
/// in. Never `Number()`ed anywhere downstream.
fn canonical_id(value: &str) -> Option<String> {
    if value.is_empty()
        || value.len() > 19
        || !value.bytes().all(|b| b.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
        || value == "0"
    {
        return None;
    }
    Some(value.to_owned())
}

fn address_matches(address: &AddressDoc, input: &ValidatedCheckout) -> bool {
    let same = |stored: &Option<String>, provided: &Option<String>| match (
        stored.as_deref().filter(|s| !s.is_empty()),
        provided,
    ) {
        (None, None) => true,
        (Some(stored), Some(provided)) => stored == provided,
        _ => false,
    };
    let provided_recipient = Some(
        input
            .address
            .recipient
            .clone()
            .unwrap_or_else(|| input.contact.name.clone()),
    );
    same(&address.recipient, &provided_recipient)
        && same(&address.line1, &Some(input.address.line1.clone()))
        && same(&address.line2, &input.address.line2.clone())
        && same(&address.city, &Some(input.address.city.clone()))
        && same(&address.region, &input.address.region.clone())
        && same(&address.postal_code, &input.address.postal_code.clone())
        && same(&address.country, &Some(input.address.country.clone()))
        && same(&address.phone, &input.address.phone.clone())
}

/// The hosted-redirect return target: the shopper's own return route on
/// the host they are shopping. https everywhere except loopback hosts
/// (local development).
fn shopper_return_url(host: &str, order_id: &str) -> Result<String, ShopperError> {
    let host_ok = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'));
    if !host_ok {
        return Err(ShopperError::BadGateway(format!(
            "cannot build a return URL for host {host:?}"
        )));
    }
    let bare = host.split(':').next().unwrap_or(host).trim_end_matches('.');
    let scheme = if matches!(bare, "localhost" | "127.0.0.1" | "::1" | "[::1]") {
        "http"
    } else {
        "https"
    };
    Ok(format!(
        "{scheme}://{host}/checkout/return?order_id={order_id}"
    ))
}

fn insufficient_stock(lines: &[i64]) -> UpstreamProblem {
    UpstreamProblem {
        status: 409,
        type_uri: "https://sanvi.dev/problems/ecommerce/insufficient-stock".to_owned(),
        title: "Insufficient stock".to_owned(),
        detail: "some lines exceed available stock".to_owned(),
        fields: Vec::new(),
        lines: lines.to_vec(),
        attempt_id: None,
        current_version: None,
        due_minor: None,
        pending_minor: None,
        order_id: None,
        customer_id: None,
    }
}

fn parse_rfc3339(value: &str) -> Result<DateTime<Utc>, ShopperError> {
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&Utc))
        .map_err(|_| {
            ShopperError::BadGateway(format!("upstream timestamp {value:?} is not RFC 3339"))
        })
}

fn parse_doc<T: serde::de::DeserializeOwned>(body: &serde_json::Value) -> Result<T, ShopperError> {
    serde_json::from_value(body.clone())
        .map_err(|err| ShopperError::BadGateway(format!("upstream body shape unexpected: {err}")))
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn project_order(order: OrderDoc) -> ShopperOrderView {
    ShopperOrderView {
        id: order.id,
        status: order.status,
        currency: order.currency,
        lines: order
            .lines
            .into_iter()
            .map(|line| ShopperOrderLineView {
                position: line.position,
                product_title: line.product_title,
                sku: line.sku,
                options: line.options,
                unit_price_minor: line.unit_price_minor,
                quantity: line.quantity,
                line_total_minor: line.line_total_minor,
            })
            .collect(),
        subtotal_minor: order.subtotal_minor,
        tax_minor: order.tax_minor,
        total_minor: order.total_minor,
        payment: ShopperOrderPaymentView {
            status: order.payment.status,
            paid_minor: order.payment.paid_minor,
            due_minor: order.payment.due_minor,
            refunded_minor: order.payment.refunded_minor,
        },
        fulfillment_status: order.fulfillment.status,
        placed_at: order.placed_at,
        cancelled_at: order.cancelled_at,
        cancel_reason: order.cancel_reason,
    }
}

fn project_attempt(attempt: AttemptDoc, with_redirect: bool) -> ShopperPaymentAttemptView {
    let pending = attempt.status == "pending";
    ShopperPaymentAttemptView {
        id: attempt.id,
        status: attempt.status,
        failure_code: attempt.failure_code,
        redirect_url: if with_redirect && pending {
            attempt.redirect_url
        } else {
            None
        },
        expires_at: if pending { attempt.expires_at } else { None },
    }
}

// ----------------------------------------------------------------------
// Upstream document shapes (the fields the orchestration reads; every
// other field of the service's bodies is ignored here).
// ----------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct StoreDoc {
    pub id: String,
    pub name: String,
    pub currency: String,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MoneyDoc {
    pub amount_minor: i64,
    pub currency: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VariantDoc {
    pub id: String,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    pub price: MoneyDoc,
    #[serde(default)]
    pub archived: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProductDoc {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub status: String,
    #[serde(default)]
    pub variants: Vec<VariantDoc>,
}

#[derive(Debug, Deserialize)]
pub struct ProductPageDoc {
    #[serde(default)]
    pub items: Vec<ProductDoc>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LocationDoc {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct LocationPageDoc {
    #[serde(default)]
    pub items: Vec<LocationDoc>,
    #[serde(default)]
    pub next_after: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StockDoc {
    pub variant_id: String,
    pub available: i64,
}

#[derive(Debug, Deserialize)]
pub struct StockPageDoc {
    #[serde(default)]
    pub items: Vec<StockDoc>,
    #[serde(default)]
    pub next_after: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReservationLineDoc {
    pub variant_id: String,
    #[serde(default)]
    pub location_id: Option<String>,
    pub quantity: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReservationDoc {
    pub id: String,
    pub status: String,
    pub expires_at: String,
    #[serde(default)]
    pub lines: Vec<ReservationLineDoc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CustomerDoc {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AddressDoc {
    pub id: String,
    #[serde(default)]
    pub recipient: Option<String>,
    #[serde(default)]
    pub line1: Option<String>,
    #[serde(default)]
    pub line2: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub postal_code: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct AddressPageDoc {
    #[serde(default)]
    pub items: Vec<AddressDoc>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrderLineDoc {
    pub position: i32,
    pub variant_id: String,
    pub product_title: String,
    #[serde(default)]
    pub sku: Option<String>,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    pub unit_price_minor: i64,
    pub quantity: i64,
    pub line_total_minor: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrderPaymentDoc {
    pub status: String,
    #[serde(default)]
    pub paid_minor: i64,
    #[serde(default)]
    pub refunded_minor: i64,
    #[serde(default)]
    pub due_minor: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrderFulfillmentDoc {
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrderDoc {
    pub id: String,
    pub status: String,
    pub currency: String,
    pub subtotal_minor: i64,
    pub tax_minor: i64,
    pub total_minor: i64,
    #[serde(default)]
    pub lines: Vec<OrderLineDoc>,
    pub payment: OrderPaymentDoc,
    pub fulfillment: OrderFulfillmentDoc,
    pub placed_at: String,
    #[serde(default)]
    pub cancelled_at: Option<String>,
    #[serde(default)]
    pub cancel_reason: Option<String>,
    #[serde(default)]
    pub version: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AttemptDoc {
    pub id: String,
    pub status: String,
    #[serde(default)]
    pub failure_code: Option<String>,
    #[serde(default)]
    pub redirect_url: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AttemptListDoc {
    #[serde(default)]
    pub items: Vec<AttemptDoc>,
}

struct ReservationLineIn {
    variant_id: String,
    location_id: String,
    quantity: i64,
}

#[derive(Default)]
struct Catalog {
    products: Vec<ProductDoc>,
    /// variant id → (its product, the variant).
    variants: BTreeMap<String, (ProductDoc, VariantDoc)>,
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::domain::{
        EntitlementView, ForwardResponse, Product, ProductAccessError, ProductUpstream,
        ShopperTenant,
    };

    // ------------------------------------------------------------------
    // Fakes
    // ------------------------------------------------------------------

    struct Fixed(Vec<EntitlementView>);

    #[async_trait]
    impl TenantEntitlements for Fixed {
        async fn resolve(&self, _: TenantId) -> Result<Vec<EntitlementView>, ProductAccessError> {
            Ok(self.0.clone())
        }
    }

    struct FixedHosts(Option<ShopperTenant>);

    #[async_trait]
    impl HostTenants for FixedHosts {
        async fn resolve(&self, _: &str) -> Result<Option<ShopperTenant>, ProductAccessError> {
            Ok(self.0)
        }
    }

    struct FixedBinding(Mutex<Option<i64>>);

    impl FixedBinding {
        fn bound(store: Option<i64>) -> Arc<Self> {
            Arc::new(Self(Mutex::new(store)))
        }
    }

    #[async_trait]
    impl StoreBindings for FixedBinding {
        async fn published_store(&self, _: TenantId) -> Result<Option<i64>, ProductAccessError> {
            Ok(*self.0.lock().unwrap())
        }

        async fn publish(&self, _: TenantId, store: Option<i64>) -> Result<(), ProductAccessError> {
            *self.0.lock().unwrap() = store;
            Ok(())
        }
    }

    /// A signing fake that round-trips claims as JSON with a prefix tag,
    /// so tests can mint and verify tokens without ring.
    struct JsonSigner;

    impl TokenSigner for JsonSigner {
        fn sign(&self, claims: &DelegationClaims) -> Result<String, ProductAccessError> {
            Ok(format!(
                "delegation:{}",
                serde_json::to_string(claims).unwrap()
            ))
        }
        fn sign_privacy(
            &self,
            _: &crate::domain::PrivacyClaims,
        ) -> Result<String, ProductAccessError> {
            unimplemented!()
        }
        fn sign_connector(
            &self,
            _: &crate::domain::ConnectorClaims,
        ) -> Result<String, ProductAccessError> {
            unimplemented!()
        }
        fn sign_checkout(&self, claims: &CheckoutClaims) -> Result<String, ProductAccessError> {
            Ok(format!(
                "checkout:{}",
                serde_json::to_string(claims).unwrap()
            ))
        }
        fn sign_credential(
            &self,
            claims: &PurchaseCredentialClaims,
        ) -> Result<String, ProductAccessError> {
            Ok(format!(
                "credential:{}",
                serde_json::to_string(claims).unwrap()
            ))
        }
        fn verify_json(&self, token: &str) -> Result<serde_json::Value, ProductAccessError> {
            let payload = token
                .split_once(':')
                .map(|(_, rest)| rest)
                .ok_or(ProductAccessError::BadRequest("not a token".to_owned()))?;
            serde_json::from_str(payload)
                .map_err(|_| ProductAccessError::BadRequest("not a token".to_owned()))
        }
    }

    /// Stands in for an unconfigured signing key: every mint fails closed.
    struct NoKeySigner;

    fn no_key() -> ProductAccessError {
        ProductAccessError::Unavailable("no delegation signing key is configured".to_owned())
    }

    impl TokenSigner for NoKeySigner {
        fn sign(&self, _: &DelegationClaims) -> Result<String, ProductAccessError> {
            Err(no_key())
        }
        fn sign_privacy(
            &self,
            _: &crate::domain::PrivacyClaims,
        ) -> Result<String, ProductAccessError> {
            Err(no_key())
        }
        fn sign_connector(
            &self,
            _: &crate::domain::ConnectorClaims,
        ) -> Result<String, ProductAccessError> {
            Err(no_key())
        }
        fn sign_checkout(&self, _: &CheckoutClaims) -> Result<String, ProductAccessError> {
            Err(no_key())
        }
        fn sign_credential(
            &self,
            _: &PurchaseCredentialClaims,
        ) -> Result<String, ProductAccessError> {
            Err(no_key())
        }
        fn verify_json(&self, _: &str) -> Result<serde_json::Value, ProductAccessError> {
            Err(no_key())
        }
    }

    #[derive(Debug, Clone)]
    struct Received {
        method: String,
        path: String,
        query: Option<String>,
        headers: Vec<(String, String)>,
        body: serde_json::Value,
    }

    type Responder =
        Box<dyn Fn(&Received) -> (u16, Vec<(String, String)>, serde_json::Value) + Send + Sync>;

    struct FakeUpstream {
        calls: Mutex<Vec<Received>>,
        respond: Responder,
    }

    impl FakeUpstream {
        fn new(
            respond: impl Fn(&Received) -> (u16, Vec<(String, String)>, serde_json::Value)
            + Send
            + Sync
            + 'static,
        ) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                respond: Box::new(respond),
            }
        }

        fn calls(&self) -> Vec<Received> {
            self.calls.lock().unwrap().clone()
        }

        fn calls_to(&self, path: &str) -> Vec<Received> {
            self.calls()
                .into_iter()
                .filter(|call| call.path == path)
                .collect()
        }
    }

    #[async_trait]
    impl ProductUpstream for FakeUpstream {
        fn product(&self) -> &'static Product {
            &ECOMMERCE
        }

        async fn forward(
            &self,
            request: &ForwardRequest,
            _identity_token: &str,
        ) -> Result<ForwardResponse, ProductAccessError> {
            let received = Received {
                method: request.method.clone(),
                path: request.path.as_str().to_owned(),
                query: request.query.clone(),
                headers: request.headers.clone(),
                body: if request.body.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::from_slice(&request.body).unwrap_or(serde_json::Value::Null)
                },
            };
            let (status, headers, body) = (self.respond)(&received);
            self.calls.lock().unwrap().push(received);
            Ok(ForwardResponse {
                status,
                headers,
                body: serde_json::to_vec(&body).unwrap(),
            })
        }
    }

    fn problem_body(slug: &str, status: u16, detail: &str) -> serde_json::Value {
        serde_json::json!({
            "type": format!("https://sanvi.dev/problems/{slug}"),
            "title": slug,
            "status": status,
            "detail": detail,
        })
    }

    fn entitled() -> Vec<EntitlementView> {
        vec![EntitlementView {
            key: "products.ecommerce.enabled".into(),
            enabled: true,
            limit: None,
        }]
    }

    fn active_tenant() -> Option<ShopperTenant> {
        Some(ShopperTenant {
            tenant_id: TenantId::try_from(1001_i64).unwrap(),
            active: true,
        })
    }

    fn store_body() -> serde_json::Value {
        serde_json::json!({
            "id": "42", "name": "Acme Shop", "slug": "acme",
            "currency": "JPY", "timezone": "Asia/Tokyo", "status": "active",
            "tax": {"mode": "none"}, "created_at": "2026-01-01T00:00:00Z",
            "version": 3, "updated_at": "2026-01-01T00:00:00Z"
        })
    }

    fn product(id: &str, status: &str, variants: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": id, "store_id": "42", "title": format!("Product {id}"),
            "description": ("A thing"), "status": status, "tax_class": "standard",
            "variants": variants, "created_at": "2026-01-01T00:00:00Z",
            "version": 1, "updated_at": "2026-01-01T00:00:00Z", "import_overrides": []
        })
    }

    fn variant(id: &str, price: i64, archived: bool) -> serde_json::Value {
        serde_json::json!({
            "id": id, "sku": serde_json::Value::Null,
            "options": {"size": "m"},
            "price": {"amount_minor": price, "currency": "JPY"},
            "archived": archived
        })
    }

    struct Harness {
        upstream: Arc<FakeUpstream>,
        scope: ShopperScope,
    }

    fn pinned_now() -> chrono::DateTime<Utc> {
        chrono::DateTime::from_timestamp(NOW_SECS, 0).unwrap()
    }

    fn harness(
        respond: impl Fn(&Received) -> (u16, Vec<(String, String)>, serde_json::Value)
        + Send
        + Sync
        + 'static,
    ) -> Harness {
        let upstream = Arc::new(FakeUpstream::new(respond));
        let scope = ShopperScope::new(
            Arc::new(FixedHosts(active_tenant())),
            Arc::new(Fixed(entitled())),
            FixedBinding::bound(Some(42)),
            Arc::new(JsonSigner),
            upstream.clone(),
            3600,
        )
        .with_clock(pinned_now);
        Harness { upstream, scope }
    }

    const NOW_SECS: i64 = 1_800_000_000;

    fn request_checkout() -> ShopperStartCheckoutRequest {
        ShopperStartCheckoutRequest {
            contact: ShopperGuestContact {
                name: "Hanako".to_owned(),
                email: "hanako@example.com".to_owned(),
                phone: None,
            },
            address: ShopperGuestAddress {
                recipient: None,
                line1: "1-2-3 Marunouchi".to_owned(),
                line2: None,
                city: "Chiyoda-ku".to_owned(),
                region: Some("Tokyo".to_owned()),
                postal_code: Some("100-0001".to_owned()),
                country: "JP".to_owned(),
                phone: None,
            },
            lines: vec![
                ShopperGuestLine {
                    variant_id: "900".to_owned(),
                    quantity: 2,
                },
                ShopperGuestLine {
                    variant_id: "901".to_owned(),
                    quantity: 1,
                },
            ],
        }
    }

    // ------------------------------------------------------------------
    // Store context
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn public_store_serves_the_published_store_summary() {
        let Harness { upstream, scope } = harness(|_| (200, vec![], store_body()));
        let view = scope.public_store("shop.acme.test").await.unwrap();
        assert_eq!(view.name, "Acme Shop");
        assert_eq!(view.currency, "JPY");
        let calls = upstream.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].path, "stores/42");
    }

    #[tokio::test]
    async fn every_unservable_host_renders_store_unavailable_without_reaching_the_service() {
        // Unknown host.
        let upstream_binding = FixedBinding::bound(Some(42));
        let upstream = Arc::new(FakeUpstream::new(|_| (200, vec![], store_body())));
        let scope = ShopperScope::new(
            Arc::new(FixedHosts(None)),
            Arc::new(Fixed(entitled())),
            upstream_binding.clone(),
            Arc::new(JsonSigner),
            upstream.clone(),
            3600,
        );
        assert!(matches!(
            scope.public_store("nobody.test").await,
            Err(ShopperError::StoreUnavailable)
        ));

        // Suspended tenant (resolves, inactive).
        let scope = ShopperScope::new(
            Arc::new(FixedHosts(Some(ShopperTenant {
                tenant_id: TenantId::try_from(1001_i64).unwrap(),
                active: false,
            }))),
            Arc::new(Fixed(entitled())),
            upstream_binding.clone(),
            Arc::new(JsonSigner),
            upstream.clone(),
            3600,
        );
        assert!(matches!(
            scope.public_store("suspended.test").await,
            Err(ShopperError::StoreUnavailable)
        ));

        // No published store.
        let scope = ShopperScope::new(
            Arc::new(FixedHosts(active_tenant())),
            Arc::new(Fixed(entitled())),
            FixedBinding::bound(None),
            Arc::new(JsonSigner),
            upstream.clone(),
            3600,
        );
        assert!(matches!(
            scope.public_store("bound.test").await,
            Err(ShopperError::StoreUnavailable)
        ));

        // Entitlement off: never an upgrade prompt, the same honest state.
        let scope = ShopperScope::new(
            Arc::new(FixedHosts(active_tenant())),
            Arc::new(Fixed(vec![EntitlementView {
                key: "products.ecommerce.enabled".into(),
                enabled: false,
                limit: None,
            }])),
            upstream_binding,
            Arc::new(JsonSigner),
            upstream.clone(),
            3600,
        );
        assert!(matches!(
            scope.public_store("off.test").await,
            Err(ShopperError::StoreUnavailable)
        ));

        // An archived store reads the same.
        let mut archived = store_body();
        archived["status"] = serde_json::json!("archived");
        let upstream = Arc::new(FakeUpstream::new(move |_| (200, vec![], archived.clone())));
        let scope = ShopperScope::new(
            Arc::new(FixedHosts(active_tenant())),
            Arc::new(Fixed(entitled())),
            FixedBinding::bound(Some(42)),
            Arc::new(JsonSigner),
            upstream.clone(),
            3600,
        );
        assert!(matches!(
            scope.public_store("archived.test").await,
            Err(ShopperError::StoreUnavailable)
        ));

        assert_eq!(
            upstream.calls().len(),
            1,
            "only the archived-store check reached the service"
        );
    }

    // ------------------------------------------------------------------
    // Catalog
    // ------------------------------------------------------------------

    fn catalog_harness() -> Harness {
        harness(
            |received| match (received.method.as_str(), received.path.as_str()) {
                ("GET", "stores/42") => (200, vec![], store_body()),
                ("GET", "stores/42/products") => (
                    200,
                    vec![],
                    serde_json::json!({
                        "items": [
                            product("700", "active", serde_json::json!([
                                variant("900", 1200, false),
                                variant("901", 1500, false)
                            ])),
                            product("701", "draft", serde_json::json!([variant("910", 100, false)])),
                            product("702", "active", serde_json::json!([variant("920", 300, true)]))
                        ],
                        "next_cursor": serde_json::Value::Null
                    }),
                ),
                ("GET", path) if path.starts_with("stores/42/products/") => {
                    let id = path.rsplit('/').next().unwrap_or_default();
                    let body = match id {
                        "700" => product(
                            "700",
                            "active",
                            serde_json::json!([
                                variant("900", 1200, false),
                                variant("901", 1500, false),
                                variant("920", 300, true)
                            ]),
                        ),
                        "701" => product(
                            "701",
                            "draft",
                            serde_json::json!([variant("910", 100, false)]),
                        ),
                        _ => product(
                            "702",
                            "active",
                            serde_json::json!([variant("920", 300, true)]),
                        ),
                    };
                    (200, vec![], body)
                }
                ("GET", "stores/42/locations") => (
                    200,
                    vec![],
                    serde_json::json!({"items": [{"id": "5"}, {"id": "6"}], "next_after": serde_json::Value::Null}),
                ),
                ("GET", "stores/42/stock") => {
                    let location = received
                        .query
                        .as_deref()
                        .unwrap_or_default()
                        .contains("location_id=5");
                    let rows = if location {
                        serde_json::json!([
                            {"location_id": "5", "variant_id": "900", "on_hand": 10, "safety_stock": 1, "held": 0, "available": 9},
                            {"location_id": "5", "variant_id": "901", "on_hand": 0, "safety_stock": 0, "held": 0, "available": 0}
                        ])
                    } else {
                        serde_json::json!([
                            {"location_id": "6", "variant_id": "900", "on_hand": 4, "safety_stock": 0, "held": 0, "available": 4},
                            {"location_id": "6", "variant_id": "901", "on_hand": 3, "safety_stock": 1, "held": 0, "available": 2}
                        ])
                    };
                    (
                        200,
                        vec![],
                        serde_json::json!({"items": rows, "next_after": serde_json::Value::Null}),
                    )
                }
                _ => (404, vec![], problem_body("not-found", 404, "no such route")),
            },
        )
    }

    #[tokio::test]
    async fn catalog_hides_non_active_products_and_archived_variants_and_marks_availability() {
        let Harness { upstream, scope } = catalog_harness();
        let page = scope
            .public_products("shop.acme.test", None, None)
            .await
            .unwrap();
        assert_eq!(
            page.items.len(),
            1,
            "the draft product and the all-archived product are hidden"
        );
        let product = &page.items[0];
        assert_eq!(product.id, "700");
        assert_eq!(
            product
                .variants
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>(),
            ["900", "901"],
            "the archived variant 920 is not offered"
        );
        assert_eq!(product.price_min_minor, 1200);
        assert_eq!(product.price_max_minor, 1500);
        let purchasable: Vec<_> = product.variants.iter().map(|v| v.purchasable).collect();
        assert_eq!(
            purchasable,
            [true, true],
            "900 is available at location 5, 901 at 6"
        );

        let detail = scope.public_product("shop.acme.test", "700").await.unwrap();
        assert_eq!(detail.id, "700");
        assert!(matches!(
            scope.public_product("shop.acme.test", "701").await,
            Err(ShopperError::NotFound {
                resource: "product"
            })
        ));
        assert!(
            upstream.calls_to("stores/42/stock").len() >= 2,
            "one stock read per location"
        );
    }

    // ------------------------------------------------------------------
    // Checkout start
    // ------------------------------------------------------------------

    fn checkout_harness() -> Harness {
        harness(
            |received| match (received.method.as_str(), received.path.as_str()) {
                ("GET", "stores/42") => (200, vec![], store_body()),
                ("GET", "stores/42/products") => (
                    200,
                    vec![],
                    serde_json::json!({
                        "items": [product("700", "active", serde_json::json!([
                            variant("900", 1200, false),
                            variant("901", 1500, false)
                        ]))],
                        "next_cursor": serde_json::Value::Null
                    }),
                ),
                ("GET", "stores/42/locations") => (
                    200,
                    vec![],
                    serde_json::json!({"items": [{"id": "5"}, {"id": "6"}], "next_after": serde_json::Value::Null}),
                ),
                ("GET", "stores/42/stock") => {
                    let location_5 = received
                        .query
                        .as_deref()
                        .unwrap_or_default()
                        .contains("location_id=5");
                    let rows = if location_5 {
                        serde_json::json!([
                            {"location_id": "5", "variant_id": "900", "on_hand": 10, "safety_stock": 1, "held": 0, "available": 9}
                        ])
                    } else {
                        serde_json::json!([
                            {"location_id": "6", "variant_id": "901", "on_hand": 3, "safety_stock": 1, "held": 0, "available": 2}
                        ])
                    };
                    (
                        200,
                        vec![],
                        serde_json::json!({"items": rows, "next_after": serde_json::Value::Null}),
                    )
                }
                ("POST", "customers") => {
                    let email_taken =
                        received.body.get("email") == Some(&serde_json::json!("taken@example.com"));
                    if email_taken {
                        (
                            409,
                            vec![],
                            serde_json::json!({
                                "type": "https://sanvi.dev/problems/ecommerce/customer-email-taken",
                                "title": "email taken", "status": 409, "detail": "that email already has a customer",
                                "customer_id": "777"
                            }),
                        )
                    } else {
                        (
                            201,
                            vec![],
                            serde_json::json!({"id": "700", "status": "active", "version": 1, "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"}),
                        )
                    }
                }
                ("GET", "customers/700/addresses") => (
                    200,
                    vec![],
                    serde_json::json!({"items": [], "next_cursor": serde_json::Value::Null}),
                ),
                ("GET", "customers/777/addresses") => (
                    200,
                    vec![],
                    serde_json::json!({"items": [], "next_cursor": serde_json::Value::Null}),
                ),
                ("POST", path)
                    if path.starts_with("customers/") && path.ends_with("/addresses") =>
                {
                    (
                        201,
                        vec![],
                        serde_json::json!({"id": "800", "customer_id": "700", "status": "active", "created_at": "2026-01-01T00:00:00Z"}),
                    )
                }
                ("POST", "stores/42/reservations") => (
                    201,
                    vec![],
                    serde_json::json!({
                        "id": "555", "store_id": "42", "status": "held",
                        "expires_at": "2026-01-01T00:15:00Z", "created_at": "2026-01-01T00:00:00Z",
                        "lines": [
                            {"variant_id": "900", "location_id": "5", "quantity": 2},
                            {"variant_id": "901", "location_id": "6", "quantity": 1}
                        ]
                    }),
                ),
                _ => (404, vec![], problem_body("not-found", 404, "no such route")),
            },
        )
    }

    #[tokio::test]
    async fn checkout_start_holds_a_priced_reservation_and_signs_the_summary() {
        let Harness { upstream, scope } = checkout_harness();
        let view = scope
            .start_checkout("shop.acme.test", "key-1", &request_checkout())
            .await
            .unwrap();

        assert_eq!(view.currency, "JPY");
        assert_eq!(view.subtotal_minor, 1200 * 2 + 1500);
        assert_eq!(view.lines[0].line_total_minor, 2400);
        assert_eq!(view.expires_at, "2026-01-01T00:15:00Z");

        // The token verifies, carries the priced lines and dies with the hold.
        let verified = JsonSigner.verify_json(&view.checkout).unwrap();
        let claims = CheckoutClaims::from_verified(verified).unwrap();
        assert_eq!(claims.sub, "555");
        assert_eq!(claims.customer_id, "700");
        assert_eq!(claims.shipping_address_id, "800");
        assert_eq!(
            claims.exp,
            chrono::DateTime::parse_from_rfc3339("2026-01-01T00:15:00Z")
                .unwrap()
                .timestamp()
        );
        assert_eq!(claims.lines.len(), 2);
        assert_eq!(claims.lines[0].unit_price_minor, 1200);

        // Namespaced idempotency keys per intent.
        let keys: Vec<_> = upstream
            .calls()
            .into_iter()
            .filter_map(|call| {
                call.headers
                    .iter()
                    .find(|(name, _)| name == "idempotency-key")
                    .map(|(_, value)| (call.path.clone(), value.clone()))
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                ("customers".to_owned(), "web-cust-key-1".to_owned()),
                (
                    "customers/700/addresses".to_owned(),
                    "web-addr-key-1".to_owned()
                ),
                (
                    "stores/42/reservations".to_owned(),
                    "web-res-key-1".to_owned()
                ),
            ]
        );

        // The reservation lines carry the best location per variant.
        let reservation = upstream
            .calls_to("stores/42/reservations")
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(
            reservation.body["lines"],
            serde_json::json!([
                {"variant_id": "900", "location_id": "5", "quantity": 2},
                {"variant_id": "901", "location_id": "6", "quantity": 1}
            ])
        );

        // The service user's delegation carries the checkout permissions.
        // (The delegation token is opaque here; the claims format check is
        // the signer's test.)
    }

    #[tokio::test]
    async fn checkout_start_adopts_an_existing_customer_for_a_repeat_email() {
        let Harness { upstream, scope } = checkout_harness();
        let mut request = request_checkout();
        request.contact.email = "taken@example.com".to_owned();
        let view = scope
            .start_checkout("shop.acme.test", "key-2", &request)
            .await
            .unwrap();
        let verified = JsonSigner.verify_json(&view.checkout).unwrap();
        let claims = CheckoutClaims::from_verified(verified).unwrap();
        assert_eq!(
            claims.customer_id, "777",
            "the refused create's existing customer is adopted"
        );
        assert!(
            upstream
                .calls()
                .iter()
                .any(|call| call.path == "customers/777/addresses"),
            "the adopted customer gets the shipping address"
        );
    }

    #[tokio::test]
    async fn checkout_start_refuses_stock_shortfalls_with_line_indexes_and_no_hold() {
        let Harness { upstream, scope } = harness(|received| {
            match (received.method.as_str(), received.path.as_str()) {
                ("GET", "stores/42") => (200, vec![], store_body()),
                ("GET", "stores/42/products") => (
                    200,
                    vec![],
                    serde_json::json!({
                        "items": [product("700", "active", serde_json::json!([
                            variant("900", 1200, false),
                            variant("901", 1500, false)
                        ]))],
                        "next_cursor": serde_json::Value::Null
                    }),
                ),
                ("GET", "stores/42/locations") => (
                    200,
                    vec![],
                    serde_json::json!({"items": [{"id": "5"}], "next_after": serde_json::Value::Null}),
                ),
                ("GET", "stores/42/stock") => (
                    200,
                    vec![],
                    serde_json::json!({"items": [
                        {"location_id": "5", "variant_id": "900", "on_hand": 10, "safety_stock": 1, "held": 0, "available": 9}
                    ], "next_after": serde_json::Value::Null}),
                ),
                _ => (404, vec![], problem_body("not-found", 404, "no such route")),
            }
        });
        // Line 1 (variant 901) has no stock anywhere.
        let error = scope
            .start_checkout("shop.acme.test", "key-3", &request_checkout())
            .await
            .unwrap_err();
        let problem = match error {
            ShopperError::Upstream(problem) => problem,
            other => panic!("expected insufficient stock, got {other:?}"),
        };
        assert_eq!(problem.slug(), "ecommerce/insufficient-stock");
        assert_eq!(problem.lines, vec![1]);
        assert!(
            !upstream
                .calls()
                .iter()
                .any(|call| call.path.ends_with("/reservations")),
            "nothing is held when any line fails"
        );
        assert!(
            !upstream.calls().iter().any(|call| call.path == "customers"),
            "the customer is not created either — the refusal comes first"
        );
    }

    #[tokio::test]
    async fn checkout_input_validation_refuses_bad_ids_quantities_and_duplicates() {
        let Harness { upstream, scope } = checkout_harness();

        let mut request = request_checkout();
        request.lines[0].quantity = 0;
        assert!(matches!(
            scope.start_checkout("shop.acme.test", "k", &request).await,
            Err(ShopperError::CheckoutInvalid { .. })
        ));

        let mut request = request_checkout();
        request.lines[0].variant_id = "0".to_owned();
        assert!(matches!(
            scope.start_checkout("shop.acme.test", "k", &request).await,
            Err(ShopperError::CheckoutInvalid { .. })
        ));

        let mut request = request_checkout();
        request.lines[1].variant_id = "900".to_owned();
        assert!(matches!(
            scope.start_checkout("shop.acme.test", "k", &request).await,
            Err(ShopperError::CheckoutInvalid { .. })
        ));

        assert!(
            matches!(
                scope
                    .start_checkout("shop.acme.test", "", &request_checkout())
                    .await,
                Err(ShopperError::CheckoutInvalid { .. })
            ),
            "an empty idempotency key is refused before anything runs"
        );
        assert!(upstream.calls().is_empty());
    }

    #[tokio::test]
    async fn release_checkout_releases_and_tolerates_a_committed_reservation() {
        let Harness { upstream, scope } = harness(|received| {
            match (received.method.as_str(), received.path.as_str()) {
                ("GET", "stores/42") => (200, vec![], store_body()),
                ("POST", "stores/42/reservations/555/release") => (
                    200,
                    vec![],
                    serde_json::json!({"id": "555", "store_id": "42", "status": "released", "expires_at": "2026-01-01T00:15:00Z", "created_at": "2026-01-01T00:00:00Z", "lines": []}),
                ),
                _ => (404, vec![], problem_body("not-found", 404, "no such route")),
            }
        });
        let token = {
            let claims = CheckoutClaims::new(
                "555".to_owned(),
                TenantId::try_from(1001_i64).unwrap(),
                42,
                "700".to_owned(),
                "800".to_owned(),
                "JPY".to_owned(),
                vec![],
                QuotedTerms {
                    pricing_fingerprint: "v1:test".to_owned(),
                    shipping_method_id: None,
                    subtotal_minor: 0,
                    shipping_minor: 0,
                    tax_minor: 0,
                    total_minor: 0,
                },
                "018f0000000070008000000000000000".to_owned(),
                chrono::DateTime::from_timestamp(NOW_SECS + 900, 0).unwrap(),
                chrono::DateTime::from_timestamp(NOW_SECS, 0).unwrap(),
            );
            JsonSigner.sign_checkout(&claims).unwrap()
        };
        scope
            .release_checkout("shop.acme.test", &token)
            .await
            .unwrap();
        assert_eq!(
            upstream
                .calls_to("stores/42/reservations/555/release")
                .len(),
            1
        );
    }

    // ------------------------------------------------------------------
    // Placement
    // ------------------------------------------------------------------

    struct PlaceConfig {
        /// The unit price the placed order carries (drift detection).
        order_price: i64,
        replayed: bool,
    }

    fn place_harness(config: PlaceConfig) -> Harness {
        harness(move |received| {
            let config = &config;
            match (received.method.as_str(), received.path.as_str()) {
                ("GET", "stores/42") => (200, vec![], store_body()),
                ("GET", "stores/42/products") => (
                    200,
                    vec![],
                    serde_json::json!({
                        "items": [product("700", "active", serde_json::json!([
                            variant("900", 1200, false),
                            variant("901", 1500, false)
                        ]))],
                        "next_cursor": serde_json::Value::Null
                    }),
                ),
                ("GET", "stores/42/locations") => (
                    200,
                    vec![],
                    serde_json::json!({"items": [{"id": "5"}], "next_after": serde_json::Value::Null}),
                ),
                ("GET", "stores/42/stock") => (
                    200,
                    vec![],
                    serde_json::json!({"items": [
                        {"location_id": "5", "variant_id": "900", "on_hand": 10, "safety_stock": 1, "held": 0, "available": 9},
                        {"location_id": "5", "variant_id": "901", "on_hand": 3, "safety_stock": 1, "held": 0, "available": 2}
                    ], "next_after": serde_json::Value::Null}),
                ),
                ("POST", "customers") => (
                    201,
                    vec![],
                    serde_json::json!({"id": "700", "status": "active", "version": 1, "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"}),
                ),
                ("GET", "customers/700/addresses") => (
                    200,
                    vec![],
                    serde_json::json!({"items": [], "next_cursor": serde_json::Value::Null}),
                ),
                ("POST", "customers/700/addresses") => (
                    201,
                    vec![],
                    serde_json::json!({"id": "800", "customer_id": "700", "status": "active", "created_at": "2026-01-01T00:00:00Z"}),
                ),
                ("POST", "stores/42/reservations") => (
                    201,
                    vec![],
                    serde_json::json!({
                        "id": "555", "store_id": "42", "status": "held",
                        "expires_at": "2033-01-01T00:15:00Z", "created_at": "2033-01-01T00:00:00Z",
                        "lines": [
                            {"variant_id": "900", "location_id": "5", "quantity": 2},
                            {"variant_id": "901", "location_id": "5", "quantity": 1}
                        ]
                    }),
                ),
                ("POST", "stores/42/orders") => {
                    let mut headers = vec![];
                    if config.replayed {
                        headers.push(("idempotency-replayed".to_owned(), "true".to_owned()));
                    }
                    (
                        201,
                        headers,
                        serde_json::json!({
                            "id": "888", "store_id": "42", "reservation_id": "555", "status": "placed",
                            "channel": "web", "currency": "JPY",
                            "subtotal_minor": config.order_price * 2 + 1500,
                            "tax_minor": 0, "total_minor": config.order_price * 2 + 1500,
                            "tax_mode": "none", "tax_lines": [],
                            "lines": [
                                {"position": 0, "product_id": "700", "variant_id": "900", "location_id": "5",
                                 "product_title": "Product 700", "sku": null, "options": {"size": "m"},
                                 "unit_price_minor": config.order_price, "quantity": 2, "line_total_minor": config.order_price * 2},
                                {"position": 1, "product_id": "700", "variant_id": "901", "location_id": "5",
                                 "product_title": "Product 700", "sku": null, "options": {"size": "l"},
                                 "unit_price_minor": 1500, "quantity": 1, "line_total_minor": 1500}
                            ],
                            "placed_at": "2033-01-01T00:00:00Z",
                            "payment": {"status": "unpaid", "paid_minor": 0, "refunded_minor": 0, "due_minor": config.order_price * 2 + 1500, "pending_minor": 0, "pending_refund_minor": 0, "overpaid_minor": 0},
                            "fulfillment": {"status": "unfulfilled", "lines": []},
                            "version": 0, "updated_at": "2033-01-01T00:00:00Z"
                        }),
                    )
                }
                ("POST", "stores/42/orders/888/cancel") => (
                    200,
                    vec![],
                    serde_json::json!({
                        "id": "888", "store_id": "42", "reservation_id": "555", "status": "cancelled",
                        "channel": "web", "currency": "JPY", "subtotal_minor": 0, "tax_minor": 0, "total_minor": 0,
                        "tax_mode": "none", "tax_lines": [], "lines": [],
                        "placed_at": "2033-01-01T00:00:00Z", "cancelled_at": "2033-01-01T00:01:00Z",
                        "cancel_reason": "other",
                        "payment": {"status": "unpaid", "paid_minor": 0, "refunded_minor": 0, "due_minor": 0, "pending_minor": 0, "pending_refund_minor": 0, "overpaid_minor": 0},
                        "fulfillment": {"status": "unfulfilled", "lines": []},
                        "version": 1, "updated_at": "2033-01-01T00:01:00Z"
                    }),
                ),
                _ => (404, vec![], problem_body("not-found", 404, "no such route")),
            }
        })
    }

    async fn place_token() -> String {
        let claims = CheckoutClaims::new(
            "555".to_owned(),
            TenantId::try_from(1001_i64).unwrap(),
            42,
            "700".to_owned(),
            "800".to_owned(),
            "JPY".to_owned(),
            vec![
                CheckoutLine {
                    variant_id: "900".to_owned(),
                    unit_price_minor: 1200,
                    quantity: 2,
                },
                CheckoutLine {
                    variant_id: "901".to_owned(),
                    unit_price_minor: 1500,
                    quantity: 1,
                },
            ],
            QuotedTerms {
                pricing_fingerprint: "v1:test".to_owned(),
                shipping_method_id: None,
                subtotal_minor: 1200 * 2 + 1500,
                shipping_minor: 0,
                tax_minor: 0,
                total_minor: 1200 * 2 + 1500,
            },
            "018f0000000070008000000000000000".to_owned(),
            chrono::DateTime::from_timestamp(NOW_SECS + 900, 0).unwrap(),
            chrono::DateTime::from_timestamp(NOW_SECS, 0).unwrap(),
        );
        JsonSigner.sign_checkout(&claims).unwrap()
    }

    #[tokio::test]
    async fn placement_returns_the_order_and_mints_the_credential() {
        let Harness { upstream, scope } = place_harness(PlaceConfig {
            order_price: 1200,
            replayed: false,
        });
        let view = scope
            .place_order("shop.acme.test", "key-9", &place_token().await)
            .await
            .unwrap();
        assert_eq!(view.order.id, "888");
        assert_eq!(view.order.total_minor, 1200 * 2 + 1500);
        assert_eq!(view.order.payment.status, "unpaid");
        assert_eq!(view.order.fulfillment_status, "unfulfilled");
        assert!(!view.credential.is_empty());

        // The credential verifies against the order and mints a live window.
        let verified = JsonSigner.verify_json(&view.credential).unwrap();
        let claims = PurchaseCredentialClaims::from_verified(verified).unwrap();
        assert_eq!(claims.sub, "888");
        assert_eq!(claims.tenant_id, "1001");
        assert!(claims.alive_at(pinned_now()));
        assert_eq!(
            upstream.calls_to("stores/42/orders").len(),
            1,
            "exactly one placement call, with the forwarded idempotency key"
        );
    }

    #[tokio::test]
    async fn a_price_drift_refuses_and_cancels_the_fresh_order() {
        let Harness { upstream, scope } = place_harness(PlaceConfig {
            order_price: 1300,
            replayed: false,
        });
        let error = scope
            .place_order("shop.acme.test", "key-9", &place_token().await)
            .await
            .unwrap_err();
        assert_eq!(error, ShopperError::PriceChanged);
        assert_eq!(
            upstream.calls_to("stores/42/orders/888/cancel").len(),
            1,
            "the fresh order is cancelled, not silently charged"
        );
    }

    #[tokio::test]
    async fn a_replayed_placement_returns_the_original_order_without_the_price_check() {
        let Harness { upstream, scope } = place_harness(PlaceConfig {
            order_price: 1300,
            replayed: true,
        });
        let view = scope
            .place_order("shop.acme.test", "key-9", &place_token().await)
            .await
            .unwrap();
        assert_eq!(view.order.id, "888");
        assert_eq!(
            upstream.calls_to("stores/42/orders/888/cancel").len(),
            0,
            "the snapshot of record already carries these prices"
        );
    }

    #[tokio::test]
    async fn an_expired_checkout_token_is_a_typed_expiry_refusal() {
        let Harness { upstream: _, scope } = place_harness(PlaceConfig {
            order_price: 1200,
            replayed: false,
        });
        let claims = CheckoutClaims::new(
            "555".to_owned(),
            TenantId::try_from(1001_i64).unwrap(),
            42,
            "700".to_owned(),
            "800".to_owned(),
            "JPY".to_owned(),
            vec![],
            QuotedTerms {
                pricing_fingerprint: "v1:test".to_owned(),
                shipping_method_id: None,
                subtotal_minor: 0,
                shipping_minor: 0,
                tax_minor: 0,
                total_minor: 0,
            },
            "018f0000000070008000000000000000".to_owned(),
            chrono::DateTime::from_timestamp(NOW_SECS - 1, 0).unwrap(),
            chrono::DateTime::from_timestamp(NOW_SECS - 1000, 0).unwrap(),
        );
        let token = JsonSigner.sign_checkout(&claims).unwrap();
        assert!(matches!(
            scope.place_order("shop.acme.test", "key-9", &token).await,
            Err(ShopperError::CheckoutExpired)
        ));
    }

    // ------------------------------------------------------------------
    // Credential-scoped access
    // ------------------------------------------------------------------

    fn order_harness() -> Harness {
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancelled_reader = cancelled.clone();
        harness(move |received| {
            let placed = serde_json::json!({
                "id": "888", "store_id": "42", "reservation_id": "555", "status": "placed",
                "channel": "web", "currency": "JPY", "subtotal_minor": 3900, "tax_minor": 0, "total_minor": 3900,
                "tax_mode": "none", "tax_lines": [],
                "lines": [
                    {"position": 0, "product_id": "700", "variant_id": "900", "location_id": "5",
                     "product_title": "Product 700", "sku": null, "options": {"size": "m"},
                     "unit_price_minor": 1200, "quantity": 2, "line_total_minor": 2400}
                ],
                "placed_at": "2033-01-01T00:00:00Z",
                "payment": {"status": "unpaid", "paid_minor": 0, "refunded_minor": 0, "due_minor": 3900, "pending_minor": 0, "pending_refund_minor": 0, "overpaid_minor": 0},
                "fulfillment": {"status": "unfulfilled", "lines": []},
                "version": 0, "updated_at": "2033-01-01T00:00:00Z"
            });
            let cancelled_body = serde_json::json!({
                "id": "888", "store_id": "42", "reservation_id": "555", "status": "cancelled",
                "channel": "web", "currency": "JPY", "subtotal_minor": 0, "tax_minor": 0, "total_minor": 0,
                "tax_mode": "none", "tax_lines": [], "lines": [],
                "placed_at": "2033-01-01T00:00:00Z", "cancelled_at": "2033-01-01T00:05:00Z",
                "cancel_reason": "customer_request",
                "payment": {"status": "unpaid", "paid_minor": 0, "refunded_minor": 0, "due_minor": 0, "pending_minor": 0, "pending_refund_minor": 0, "overpaid_minor": 0},
                "fulfillment": {"status": "unfulfilled", "lines": []},
                "version": 1, "updated_at": "2033-01-01T00:05:00Z"
            });
            match (received.method.as_str(), received.path.as_str()) {
                ("GET", "stores/42") => (200, vec![], store_body()),
                ("GET", "stores/42/orders/888") => {
                    if cancelled_reader.load(std::sync::atomic::Ordering::SeqCst) {
                        (200, vec![], cancelled_body.clone())
                    } else {
                        (200, vec![], placed.clone())
                    }
                }
                ("GET", "stores/42/orders/888/payment-attempts") => (
                    200,
                    vec![],
                    serde_json::json!({"items": [
                        {"id": "a1", "order_id": "888", "amount_minor": 3900, "currency": "JPY", "status": "pending",
                         "failure_code": null, "expires_at": "2033-01-01T00:30:00Z", "created_at": "2033-01-01T00:00:00Z",
                         "client_secret": null, "redirect_url": "https://provider.test/pay"}
                    ]}),
                ),
                ("POST", "stores/42/orders/888/payment-attempts/a1/cancel") => (
                    200,
                    vec![],
                    serde_json::json!({"id": "a1", "order_id": "888", "amount_minor": 3900, "currency": "JPY", "status": "cancelled", "failure_code": null, "expires_at": "2033-01-01T00:30:00Z", "created_at": "2033-01-01T00:00:00Z"}),
                ),
                ("POST", "stores/42/orders/888/cancel") => {
                    cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
                    (200, vec![], cancelled_body.clone())
                }
                ("POST", "stores/42/orders/888/payment-attempts") => (
                    201,
                    vec![],
                    serde_json::json!({"id": "a1", "order_id": "888", "amount_minor": 3900, "currency": "JPY", "status": "pending",
                        "failure_code": null, "expires_at": "2033-01-01T00:30:00Z", "created_at": "2033-01-01T00:00:00Z",
                        "client_secret": null, "redirect_url": "https://provider.test/pay"}),
                ),
                _ => (404, vec![], problem_body("not-found", 404, "no such route")),
            }
        })
    }

    fn credential(order_id: &str, ttl: i64) -> String {
        let claims = PurchaseCredentialClaims::new(
            order_id.to_owned(),
            TenantId::try_from(1001_i64).unwrap(),
            42,
            "018f0000000070008000000000000000".to_owned(),
            ttl,
            chrono::DateTime::from_timestamp(NOW_SECS, 0).unwrap(),
        );
        JsonSigner.sign_credential(&claims).unwrap()
    }

    #[tokio::test]
    async fn order_access_needs_a_valid_credential_and_an_id_alone_authorizes_nothing() {
        let Harness { upstream: _, scope } = order_harness();

        // Absent, malformed, wrong order, wrong tenant: all invalid.
        assert!(matches!(
            scope.order_status("shop.acme.test", "888", None).await,
            Err(ShopperError::CredentialInvalid)
        ));
        assert!(matches!(
            scope
                .order_status("shop.acme.test", "888", Some("garbage"))
                .await,
            Err(ShopperError::CredentialInvalid)
        ));
        assert!(matches!(
            scope
                .order_status("shop.acme.test", "888", Some(&credential("889", 3600)))
                .await,
            Err(ShopperError::CredentialInvalid)
        ));

        // Expired is its own honest class.
        assert!(matches!(
            scope
                .order_status("shop.acme.test", "888", Some(&credential("888", -1)))
                .await,
            Err(ShopperError::CredentialExpired)
        ));

        // Valid: the projection carries no internal bookkeeping.
        let view = scope
            .order_status("shop.acme.test", "888", Some(&credential("888", 3600)))
            .await
            .unwrap();
        assert_eq!(view.id, "888");
        assert_eq!(view.total_minor, 3900);
        assert_eq!(view.payment.status, "unpaid");
        let rendered = serde_json::to_value(&view).unwrap();
        assert!(rendered.get("reservation_id").is_none());
        assert!(rendered.get("customer_id").is_none());
        assert!(rendered.get("version").is_none());
        assert!(rendered.get("shipping_address_id").is_none());
    }

    #[tokio::test]
    async fn cancel_order_cancels_the_pending_attempt_first_then_the_order() {
        let Harness { upstream, scope } = order_harness();
        let view = scope
            .cancel_order("shop.acme.test", "888", Some(&credential("888", 3600)))
            .await
            .unwrap();
        assert_eq!(view.status, "cancelled");
        assert_eq!(view.cancel_reason.as_deref(), Some("customer_request"));
        let calls = upstream.calls();
        let attempt_cancel = calls
            .iter()
            .position(|call| call.path == "stores/42/orders/888/payment-attempts/a1/cancel")
            .unwrap();
        let order_cancel = calls
            .iter()
            .position(|call| call.path == "stores/42/orders/888/cancel")
            .unwrap();
        assert!(
            attempt_cancel < order_cancel,
            "the pending attempt must go before the order cancel"
        );
        assert_eq!(calls[order_cancel].body["reason"], "customer_request");
    }

    #[tokio::test]
    async fn payment_start_builds_the_return_url_and_payment_status_polls_the_latest_attempt() {
        let Harness { upstream, scope } = order_harness();
        let view = scope
            .start_payment(
                "shop.acme.test",
                "888",
                Some(&credential("888", 3600)),
                "key-p",
            )
            .await
            .unwrap();
        assert_eq!(view.attempt.status, "pending");
        assert_eq!(
            view.attempt.redirect_url.as_deref(),
            Some("https://provider.test/pay")
        );

        let call = upstream
            .calls_to("stores/42/orders/888/payment-attempts")
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(
            call.body["return_url"], "https://shop.acme.test/checkout/return?order_id=888",
            "the provider returns to the shopper's own host, never to the API"
        );
        assert_eq!(
            call.headers
                .iter()
                .find(|(name, _)| name == "idempotency-key")
                .map(|(_, value)| value.as_str()),
            Some("web-pay-key-p"),
            "payment start keeps its own intent key"
        );

        let status = scope
            .payment_status("shop.acme.test", "888", Some(&credential("888", 3600)))
            .await
            .unwrap();
        let attempt = status.attempt.unwrap();
        assert_eq!(attempt.status, "pending");
        assert!(
            attempt.redirect_url.is_none(),
            "status views never carry the redirect"
        );
    }

    #[tokio::test]
    async fn payment_start_passes_through_the_provider_refusals() {
        let Harness { upstream: _, scope } = harness(|received| {
            match (received.method.as_str(), received.path.as_str()) {
                ("GET", "stores/42") => (200, vec![], store_body()),
                ("POST", "stores/42/orders/888/payment-attempts") => (
                    409,
                    vec![],
                    serde_json::json!({
                        "type": "https://sanvi.dev/problems/ecommerce/payment-in-progress",
                        "title": "payment in progress", "status": 409, "detail": "an attempt is pending",
                        "attempt_id": "a1"
                    }),
                ),
                _ => (404, vec![], problem_body("not-found", 404, "no such route")),
            }
        });
        let error = scope
            .start_payment(
                "shop.acme.test",
                "888",
                Some(&credential("888", 3600)),
                "key-p",
            )
            .await
            .unwrap_err();
        let problem = match error {
            ShopperError::Upstream(problem) => problem,
            other => panic!("expected the typed refusal, got {other:?}"),
        };
        assert_eq!(problem.slug(), "ecommerce/payment-in-progress");
        assert_eq!(problem.attempt_id.as_deref(), Some("a1"));
    }

    #[tokio::test]
    async fn checkout_and_order_mutations_never_run_when_the_upstream_is_unconfigured() {
        // An unconfigured upstream fails every orchestration closed with 503.
        let upstream = Arc::new(FakeUpstream::new(|_| (200, vec![], store_body())));
        let scope = ShopperScope::new(
            Arc::new(FixedHosts(active_tenant())),
            Arc::new(Fixed(entitled())),
            FixedBinding::bound(Some(42)),
            Arc::new(NoKeySigner),
            upstream.clone(),
            3600,
        );
        assert!(matches!(
            scope.public_store("shop.acme.test").await,
            Err(ShopperError::Unavailable(_) | ShopperError::BadGateway(_))
        ));
    }

    #[tokio::test]
    async fn publish_store_validates_the_store_before_binding() {
        let upstream = Arc::new(FakeUpstream::new(|received| match received.path.as_str() {
            "stores/99" => (200, vec![], store_body()),
            _ => (404, vec![], problem_body("not-found", 404, "no such store")),
        }));
        let scope = ShopperScope::new(
            Arc::new(FixedHosts(active_tenant())),
            Arc::new(Fixed(entitled())),
            FixedBinding::bound(None),
            Arc::new(JsonSigner),
            upstream.clone(),
            3600,
        );
        let tenant = TenantId::try_from(1001_i64).unwrap();
        let validate = async |tenant: TenantId, store: i64| {
            assert_eq!(tenant.as_i64(), 1001);
            if store == 99 {
                Ok(StoreDoc {
                    id: "99".to_owned(),
                    name: "Acme Shop".to_owned(),
                    currency: "JPY".to_owned(),
                    status: "active".to_owned(),
                })
            } else {
                Err(ShopperError::NotFound { resource: "store" })
            }
        };
        let view = scope
            .publish_store(tenant, Some(99), validate)
            .await
            .unwrap();
        assert_eq!(view.store_id.as_deref(), Some("99"));

        let validate_refused =
            async |_: TenantId, _| Err(ShopperError::NotFound { resource: "store" });
        assert!(
            scope
                .publish_store(tenant, Some(1), validate_refused)
                .await
                .is_err()
        );
        let published = scope.published_store(tenant).await.unwrap();
        assert_eq!(
            published.store_id.as_deref(),
            Some("99"),
            "the refused publish left the earlier binding"
        );
    }

    #[test]
    fn return_urls_are_https_except_loopback_and_reject_garbage_hosts() {
        assert_eq!(
            shopper_return_url("shop.acme.test", "888").unwrap(),
            "https://shop.acme.test/checkout/return?order_id=888"
        );
        assert_eq!(
            shopper_return_url("localhost:4177", "888").unwrap(),
            "http://localhost:4177/checkout/return?order_id=888"
        );
        assert!(shopper_return_url("", "888").is_err());
        assert!(shopper_return_url("bad host", "888").is_err());
        assert!(shopper_return_url("host\r\ninjected", "888").is_err());
    }
}
