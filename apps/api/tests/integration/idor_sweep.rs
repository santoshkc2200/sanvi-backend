//! Cross-tenant IDOR sweep (TASK-024 slice 11.5b), against the assembled
//! app and a real Postgres.
//!
//! The swept surface is **enumerated, not hand-listed**: the committed
//! route security matrix (`docs/security/route-metadata.json`, produced by
//! slice 11.5a) drives every request, so routes phase 10 and later land
//! are covered by this suite as soon as they appear in the matrix. Three
//! sweeps run:
//!
//! 1. **Foreign host** — the attacker's session (member of tenant A only)
//!    hits every `CurrentTenant` route with tenant B as the resolved
//!    tenant. Permission-gated routes must answer 403 naming the route's
//!    declared permission. This sweep proves both that the route is
//!    mounted and that the permission gate consults tenant-scoped
//!    membership: the Keto stub allows exactly the attacker's tenant-A
//!    role objects, so a 403 here is the *membership* denial, not an
//!    outage (an unreachable Keto would surface as 503 and fail).
//! 2. **Cross-tenant ids** — the attacker (a genuine admin inside tenant
//!    A) requests tenant B's real resource ids on tenant A's own host.
//!    The gate deliberately allows these; storage scoping must answer
//!    404 or 403 without echoing anything from the foreign row. Every
//!    route is also probed with a nonexistent id and both responses must
//!    carry the same status, so a 404 that only fires for unknown ids (an
//!    existence oracle) fails the sweep.
//! 3. **Platform routes** — the tenant-scoped session hits every mounted
//!    platform administration route; the gate must deny with the declared
//!    platform permission named.
//!
//! Everything tenant B owns is seeded with a distinctive marker string;
//! no response to any attacker request may contain it. A per-request
//! timing assertion is too flaky for CI; the twin-id status comparison
//! above is the automatable half of the no-existence-leak requirement,
//! and the timing half belongs to the parked penetration test.
//!
//! Out of scope, by matrix classification rather than by omission: routes
//! the matrix classifies as self-service (user/session scope, covered by
//! the identity context's own suites), public platform data, webhooks
//! (signature-verified, covered per context), `/internal` (network
//!-isolated, self-authenticated), and the billing/privacy *platform*
//! routes (those contexts are deliberately not mounted here; their
//! isolation is covered by their own integration suites). The coverage
//! test at the bottom pins this: every `CurrentTenant` route in the
//! matrix is swept by sweep 1, and every id-bearing one by sweep 2 unless
//! it appears in the explicitly-reasoned exclusion list.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use actix_web::test as aw_test;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};

use sanvi_api::{
    AccessEntitlementAdmin, AppConfig, DomainsTenantLookupAdapter, FallbackGuideTranslator,
    IdentityDependencies, PlatformAdminDependencies, TenantCatalogAdapter, build_access_state,
    build_advertising_state, build_app, build_audit_state, build_domains_lookup,
    build_domains_state, build_identity_state, build_localization_state, build_payments_state,
    build_platform_admin_state, build_system_state, build_tenancy_state, build_theming_state,
};
use sanvi_persistence::TxManager;
use sanvi_privacy::domain::{
    DirectiveResolver, DirectiveSnapshot, DirectiveSource, DirectiveState, JurisdictionRef,
    PrivacyError, ProcessingPurpose, PurposeDirective, SubjectRef,
};
use sanvi_system::domain::BuildInfo;
use sanvi_telemetry::MetricsRegistry;

// ---------------------------------------------------------------------------
// Fixed identities. The 910_231_xxx range is owned by this suite; nothing
// else may mint ids in it.
// ---------------------------------------------------------------------------

const TENANT_A: i64 = 910_231_001;
const TENANT_B: i64 = 910_231_002;
const ATTACKER: i64 = 910_231_010;
const MEMBER_B: i64 = 910_231_011;
const ROLE_A: i64 = 910_231_020;
const ROLE_B: i64 = 910_231_021;

const AD_CONNECTION: i64 = 910_231_100;
const AD_CAMPAIGN: i64 = 910_231_101;
const AD_GROUP: i64 = 910_231_102;
const AD: i64 = 910_231_103;
const CREATIVE: i64 = 910_231_104;
const AUDIENCE: i64 = 910_231_105;
const CONVERSION: i64 = 910_231_106;
const BUDGET_ALERT: i64 = 910_231_107;
const PAY_CONNECTION: i64 = 910_231_110;
const CHECKOUT: i64 = 910_231_111;
const PAYMENT: i64 = 910_231_112;
const DOMAIN: i64 = 910_231_120;
const ORDER: i64 = 910_231_121;
const INVITATION: i64 = 910_231_122;

/// Substituted for every entity-id slot in the twin probe of sweep 2: an
/// id that exists nowhere.
const UNKNOWN_ID: &str = "910999999";

/// Every tenant B resource carries this substring; no attacker response
/// may echo it.
const MARKER: &str = "tenant-b-marker-9f2c";

const ATTACKER_EMAIL: &str = "attacker@tenant-a.itest";
const ATTACKER_KRATOS_ID: &str = "910231b0-0000-7000-8000-0000000aa001";
const SESSION_TOKEN: &str = "idor-sweep-attacker-session-token";
const SESSION_COOKIE: &str = "ory_kratos_session";

const BASE_DOMAIN: &str = "itest";
const HOST_A: &str = "tenant-a.itest";
const HOST_B: &str = "tenant-b.itest";

// ---------------------------------------------------------------------------
// Route matrix loading
// ---------------------------------------------------------------------------

struct MatrixRow {
    method: String,
    path: String,
    handler: String,
    permission: String,
    tenant: String,
}

fn matrix() -> Vec<MatrixRow> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/security/route-metadata.json");
    let raw: Value = serde_json::from_str(
        &std::fs::read_to_string(path).expect("route-metadata.json is committed by slice 11.5a"),
    )
    .expect("route-metadata.json parses");
    raw["routes"]
        .as_array()
        .expect("routes array")
        .iter()
        .map(|row| MatrixRow {
            method: row["method"].as_str().expect("method").to_string(),
            path: row["path"].as_str().expect("path").to_string(),
            handler: row["handler"].as_str().expect("handler").to_string(),
            permission: row["permission"].as_str().expect("permission").to_string(),
            tenant: row["tenant"].as_str().expect("tenant").to_string(),
        })
        .collect()
}

impl MatrixRow {
    fn is_current_tenant(&self) -> bool {
        self.tenant == "CurrentTenant"
    }

    fn is_platform(&self) -> bool {
        self.tenant.starts_with("platform cross-tenant")
    }

    /// The context crate owning the handler ("app" for api-level routes).
    fn context(&self) -> &str {
        let parts: Vec<&str> = self.handler.split('/').collect();
        if parts.first() == Some(&"crates") {
            parts[2]
        } else {
            "app"
        }
    }

    /// The declared permission, unless the row is an explicit exemption.
    fn declared_permission(&self) -> Option<&str> {
        if self.permission.starts_with("exempt:") {
            None
        } else {
            Some(&self.permission)
        }
    }
}

// ---------------------------------------------------------------------------
// Seeded tenant B and the slot tables
// ---------------------------------------------------------------------------

struct Seeded {
    ad_connection: i64,
    campaign: i64,
    ad_group: i64,
    ad: i64,
    creative: i64,
    audience: i64,
    conversion: i64,
    budget_alert: i64,
    pay_connection: i64,
    checkout: i64,
    payment: i64,
    domain: i64,
    order: i64,
    invitation: i64,
    member_b: i64,
    media_asset: &'static str,
}

impl Default for Seeded {
    fn default() -> Self {
        Self {
            ad_connection: AD_CONNECTION,
            campaign: AD_CAMPAIGN,
            ad_group: AD_GROUP,
            ad: AD,
            creative: CREATIVE,
            audience: AUDIENCE,
            conversion: CONVERSION,
            budget_alert: BUDGET_ALERT,
            pay_connection: PAY_CONNECTION,
            checkout: CHECKOUT,
            payment: PAYMENT,
            domain: DOMAIN,
            order: ORDER,
            invitation: INVITATION,
            member_b: MEMBER_B,
            media_asset: "idor-asset-b",
        }
    }
}

impl Seeded {
    /// Every entity id replaced by one that exists nowhere: the twin
    /// probe of sweep 2.
    fn unknown_twin() -> Seeded {
        Seeded {
            ad_connection: UNKNOWN_ID_ID,
            campaign: UNKNOWN_ID_ID,
            ad_group: UNKNOWN_ID_ID,
            ad: UNKNOWN_ID_ID,
            creative: UNKNOWN_ID_ID,
            audience: UNKNOWN_ID_ID,
            conversion: UNKNOWN_ID_ID,
            budget_alert: UNKNOWN_ID_ID,
            pay_connection: UNKNOWN_ID_ID,
            checkout: UNKNOWN_ID_ID,
            payment: UNKNOWN_ID_ID,
            domain: UNKNOWN_ID_ID,
            order: UNKNOWN_ID_ID,
            invitation: UNKNOWN_ID_ID,
            member_b: UNKNOWN_ID_ID,
            media_asset: "idor-unknown-asset",
        }
    }
}

/// `UNKNOWN_ID` as an id-sized value for the twin probe.
const UNKNOWN_ID_ID: i64 = 910_999_999;

/// The value a `{slot}` in a matrix path is filled with for real
/// requests. Enum and free-text slots get valid non-id values.
fn fill_slot(path: &str, slot: &str, seeded: &Seeded) -> String {
    entity_id(path, slot, seeded)
        .unwrap_or_else(|| match slot {
            "platform" => "google_ads".to_string(),
            "kind" => "colors".to_string(),
            "key" => "default_locale".to_string(),
            _ => UNKNOWN_ID.to_string(),
        })
        .to_string()
}

/// The tenant-B entity id a slot addresses, if the slot is an entity id
/// at all. `None` marks enum/free-text slots (sweep 2 skips those
/// routes — there is no foreign id to substitute).
fn entity_id(path: &str, slot: &str, seeded: &Seeded) -> Option<String> {
    let value = match (path, slot) {
        (p, "campaign_id") if p.contains("/ads/") => seeded.campaign,
        (p, "ad_group_id") if p.contains("/ads/") => seeded.ad_group,
        (p, "ad_id") if p.contains("/ads/") => seeded.ad,
        (p, "connection_id") if p.contains("/ads/connections") => seeded.ad_connection,
        (p, "creative_id") if p.contains("/ads/creatives") => seeded.creative,
        (p, "id") if p.contains("/ads/audiences") => seeded.audience,
        (p, "id") if p.contains("/ads/conversions") => seeded.conversion,
        (p, "alert_id") if p.contains("/ads/budget-alerts") => seeded.budget_alert,
        (p, "id") if p.contains("/payments/connections") => seeded.pay_connection,
        (p, "id") if p.contains("/checkout") => seeded.checkout,
        (p, "id") if p.contains("/payments/") => seeded.payment,
        (p, "id") if p.contains("/domains/orders") => seeded.order,
        (p, "id") if p.contains("/domains/") => seeded.domain,
        (_, "invitation_id") => seeded.invitation,
        (_, "user_id") => seeded.member_b,
        (p, "id") if p.contains("/roles/") => ROLE_B,
        (_, "asset_id") => return Some(seeded.media_asset.to_string()),
        (p, "id") if p.contains("/platform/tenants") => TENANT_A,
        _ => return None,
    };
    Some(value.to_string())
}

fn slots_of(path: &str) -> Vec<&str> {
    path.match_indices('{')
        .filter_map(|(start, _)| {
            let end = path[start..].find('}')? + start;
            Some(&path[start + 1..end])
        })
        .collect()
}

fn materialize(path: &str, seeded: &Seeded) -> String {
    let mut out = path.to_string();
    for slot in slots_of(path) {
        out = out.replace(&format!("{{{slot}}}"), &fill_slot(path, slot, seeded));
    }
    out
}

/// Minimal *deserializable and valid* request bodies, keyed by the
/// matrix path pattern. Actix extracts `Json` before `RequiredPermission`
/// on several handlers, so sweep 1 needs a body the extractor accepts to
/// even reach the permission denial; sweep 2 needs one valid enough that
/// the only thing that can fail is the cross-tenant lookup. Routes not
/// matched take no body.
fn query_suffix(pattern: &str) -> &'static str {
    // `Query` extractors run before the permission guard on these routes:
    // without deserializable params they answer 400 before the gate.
    match pattern {
        p if p.starts_with("/api/v1/tenant/ads/metrics") => "?from=2026-09-01&to=2026-09-30",
        "/api/v1/tenant/ads/campaigns/{campaign_id}/budget-cap" => "?period=monthly",
        "/api/v1/tenant/ads/creatives/{creative_id}/previews" => "?placement=feed",
        "/api/v1/tenant/ads/spend-status" => "?period=monthly",
        "/api/v1/tenant/domains/search" => "?q=sweep.example",
        "/api/v1/tenant/domains/{id}/instructions" => "?locale=en",
        "/api/v1/tenant/theme/preview" => "?token=idor-sweep&locale=en",
        "/api/v1/tenant/ads/connections/{platform}/oauth/callback" => {
            "?state=idor-sweep&code=idor-sweep&redirect_uri=https%3A%2F%2Ftenant-a.itest%2Fads%2Foauth%2Fcallback"
        }
        _ => "",
    }
}

/// Minimal *deserializable and valid* request bodies, keyed by the
/// matrix path pattern. Actix extracts `Json` before `RequiredPermission`
/// on several handlers, so sweep 1 needs a body the extractor accepts to
/// even reach the permission denial; sweep 2 needs one valid enough that
/// the only thing that can fail is the cross-tenant lookup. Routes not
/// matched take no body (their handlers carry no `Json` extractor).
fn body_for(method: &str, pattern: &str) -> Option<Value> {
    if method == "GET" || method == "DELETE" {
        return None;
    }
    match pattern {
        "/api/v1/tenant/ads/audiences" => {
            Some(json!({"name": "sweep audience", "platform": "google_ads"}))
        }
        "/api/v1/tenant/ads/budget-caps" => {
            Some(json!({"period": "monthly", "amount": {"amount_minor": 5000, "currency": "JPY"}}))
        }
        "/api/v1/tenant/ads/campaigns" => Some(json!({
            "connection_id": AD_CONNECTION.to_string(),
            "name": "sweep campaign",
            "objective": "sales",
            "budget": {"kind": "daily", "amount": {"amount_minor": 1000, "currency": "JPY"}}
        })),
        "/api/v1/tenant/ads/campaigns/{campaign_id}/ad-groups" => Some(json!({
            "name": "sweep ad group",
            "targeting": {"dimensions": {}, "extension_fields": {}},
            "bid": {"strategy": "manual"}
        })),
        p if p.ends_with("/ad-groups/{ad_group_id}/ads") => Some(json!({
            "creative": {"id": "idor-asset-b", "placement": "feed",
                         "texts": [{"locale": "en", "headline": "sweep", "body": "sweep body"}],
                         "asset_references": []},
            "landing_url": "https://example.com/sweep"
        })),
        p if p.contains("/ad-groups/{ad_group_id}/ads/{ad_id}") => {
            Some(json!({"landing_url": "https://example.com/sweep"}))
        }
        "/api/v1/tenant/ads/campaigns/{campaign_id}/budget-cap" => {
            Some(json!({"period": "monthly", "amount": {"amount_minor": 5000, "currency": "JPY"}}))
        }
        p if p.contains("/ad-groups/{ad_group_id}") => Some(json!({"name": "sweep ad group"})),
        "/api/v1/tenant/ads/campaigns/{campaign_id}" => Some(json!({"name": "sweep campaign"})),
        "/api/v1/tenant/ads/campaigns/{campaign_id}/validate" => {
            Some(json!({"name": "sweep campaign"}))
        }
        "/api/v1/tenant/ads/campaigns/{campaign_id}/drift" => {
            Some(json!({"revision": 1, "resolution": "reapply_ours"}))
        }
        "/api/v1/tenant/ads/connections" => Some(json!({
            "connection_id": "910231501",
            "external_account_id": "acct-idor-sweep",
            "currency": "JPY",
            "timezone": "Asia/Tokyo"
        })),
        p if p.ends_with("/oauth/start") => {
            Some(json!({"redirect_uri": format!("https://{HOST_A}/ads/oauth/callback")}))
        }
        "/api/v1/tenant/ads/creatives" => Some(json!({
            "connection_id": AD_CONNECTION.to_string(),
            "placement": "feed",
            "texts": [{"locale": "en", "headline": "sweep", "body": "sweep body"}],
            "asset_references": []
        })),
        "/api/v1/tenant/ads/tracking/settings" => {
            Some(json!({"tenant_id": TENANT_A.to_string(), "mappings": {}}))
        }
        "/api/v1/tenant/ads/tracking/test-event" => {
            Some(json!({"event_name": "purchase", "event_id": "idor-sweep-evt"}))
        }
        "/api/v1/tenant/checkout" => Some(json!({
            "reference": "idor-sweep-order",
            "amount_minor": 100,
            "currency": "JPY",
            "customer_ref": "idor-sweep-customer",
            "success_url": format!("https://{HOST_A}/ok"),
            "cancel_url": format!("https://{HOST_A}/cancel")
        })),
        "/api/v1/tenant/domains" => Some(json!({"hostname": "sweep.b.example"})),
        "/api/v1/tenant/domains/orders" => Some(json!({
            "hostname": "sweep-order.b.example",
            "registrant_contact": {
                "name": "idor sweep", "email": "sweep@example.com",
                "phone": "+10000000000", "country_code": "US"
            }
        })),
        p if p.ends_with("/auto-renew") => Some(json!({"enabled": true})),
        // The attacker works with their own tenant's roles everywhere;
        // only the target resources are foreign.
        "/api/v1/tenant/invitations" => Some(json!({
            "email": "idor-sweep-invitee@example.com",
            "role_ids": [ROLE_A.to_string()]
        })),
        "/api/v1/tenant/invitations/accept" => Some(json!({"token": "idor-sweep"})),
        "/api/v1/tenant/members" => Some(json!({
            "email": "idor-sweep-member@example.com",
            "role_ids": [ROLE_A.to_string()]
        })),
        "/api/v1/tenant/localization/settings" => {
            Some(json!({"default_locale": "en", "enabled_locales": ["en"], "timezone": "UTC"}))
        }
        "/api/v1/tenant/media/v1/assets" => Some(
            json!({"kind": "image", "content_type": "image/png", "filename": "sweep.png", "size_bytes": 1}),
        ),
        p if p.ends_with("/parts") => Some(json!({"part_numbers": [1]})),
        p if p.ends_with("/complete") => Some(json!({"parts": []})),
        "/api/v1/tenant/payments/connections" => {
            Some(json!({"provider": "stripe_connect", "country": "JP", "default_currency": "JPY"}))
        }
        "/api/v1/tenant/payments/tax-settings" => Some(json!({"enabled": false})),
        p if p.ends_with("/refund") => Some(json!({"reason": "idor-sweep"})),
        // The attacker administers tenant A, so the role they would grant
        // is their own tenant's; the wrong tenant only shows up in the
        // target member. (Snowflake ids deserialize from decimal strings
        // per the id policy.)
        p if p.ends_with("/members/{user_id}/roles") => {
            Some(json!({"role_ids": [ROLE_A.to_string()]}))
        }
        "/api/v1/tenant/roles" => {
            Some(json!({"key": "idor_sweep_role", "name": "sweep role", "permissions": []}))
        }
        p if p.ends_with("/roles/{id}") => Some(json!({"name": "sweep role"})),
        "/api/v1/tenant/localization/overrides" => {
            Some(json!([{"key": "idor.sweep", "locale": "en", "value": "sweep"}]))
        }
        "/api/v1/tenant/settings" => Some(json!({"settings": {}})),
        "/api/v1/tenant/theme/assets" => Some(json!({"kind": "logo", "data_base64": "aGVsbG8="})),
        "/api/v1/tenant/theme/draft" => Some(json!({})),
        "/api/v1/platform/features" => {
            Some(json!({"key": "idor.sweep", "name": "sweep feature", "kind": "boolean"}))
        }
        "/api/v1/platform/features/{key}" => {
            Some(json!({"name": "sweep feature", "default_enabled": false}))
        }
        "/api/v1/platform/impersonations" => Some(json!({
            "tenant_id": TENANT_B.to_string(),
            "target_user_id": MEMBER_B.to_string(),
            "reason": "idor-sweep",
            "duration_minutes": 30
        })),
        "/api/v1/platform/tenants" => Some(
            json!({"slug": "idor-sweep-tenant", "display_name": "idor sweep tenant", "region": "us"}),
        ),
        "/api/v1/platform/tenants/{id}/application-fee" => {
            Some(json!({"enabled": true, "minimum_minor": 0}))
        }
        "/api/v1/platform/tenants/{id}/offboarding" => {
            Some(json!({"idempotency_key": "idor-sweep"}))
        }
        "/api/v1/platform/tenants/{id}/entitlements" => {
            Some(json!({"feature_key": "payments.stripe_connect", "enabled": true}))
        }
        "/api/v1/platform/tenants/{id}/suspend" => Some(json!({"reason": "idor-sweep"})),
        p if p.ends_with("/release") => Some(json!({"reason": "idor-sweep"})),
        "/api/v1/platform/themes" => Some(json!({
            "manifest": {}, "tokens": {},
            "artifact_uri": "https://example.com/sweep.tar",
            "artifact_digest": "sha256:idor-sweep",
            "publisher": "idor-sweep", "visibility": "private",
            "allow_contrast_warnings": false
        })),
        "/api/v1/platform/localization/translations" => {
            Some(json!([{"key": "idor.sweep", "locale": "en", "value": "sweep"}]))
        }
        // Every remaining mutating platform handler takes no body (or all
        // optional fields): `{}` deserializes and the gate runs first.
        p if p.starts_with("/api/v1/platform/") => Some(json!({})),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Stub backends: Kratos (authn), Keto (authz), hitox (media upstream).
//
// Small hand-rolled HTTP/1.1 responders on ephemeral ports: the stubs
// answer from the request head alone, so a full HTTP stack would buy
// nothing (and `actix-test` is not a workspace dependency).

type StubReply = (u16, &'static str, Vec<u8>);

async fn spawn_stub<F, Fut>(handler: F) -> String
where
    F: Fn(String, String, BTreeMap<String, String>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = StubReply> + Send,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            // One request per connection (`connection: close` below): read
            // the head, drain any body so it cannot trigger a reset before
            // the response lands, answer, close.
            let mut head_end = None;
            let mut buf = Vec::with_capacity(2048);
            let mut chunk = [0u8; 2048];
            loop {
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            head_end = Some(pos);
                            break;
                        }
                    }
                }
            }
            let Some(head_end) = head_end else { continue };
            let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            let mut lines = head.lines();
            let request_line = lines.next().unwrap_or_default();
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or("GET").to_string();
            let target = parts.next().unwrap_or("/").to_string();
            let mut headers = BTreeMap::new();
            for line in lines {
                if let Some((name, value)) = line.split_once(':') {
                    headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                }
            }
            let body_buffered = buf.len().saturating_sub(head_end + 4);
            let content_length = headers
                .get("content-length")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            let mut remaining = content_length.saturating_sub(body_buffered);
            while remaining > 0 {
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => remaining = remaining.saturating_sub(n),
                }
            }

            let (status, content_type, body) = handler(method, target, headers).await;
            let reason = match status {
                200 => "OK",
                204 => "No Content",
                401 => "Unauthorized",
                403 => "Forbidden",
                404 => "Not Found",
                _ => "Error",
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
        }
    });
    format!("http://{addr}")
}

async fn kratos_stub(
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
) -> StubReply {
    if method == "GET" && target == "/sessions/whoami" {
        let cookie = headers
            .get("cookie")
            .map(String::as_str)
            .unwrap_or_default();
        if cookie.contains(SESSION_TOKEN) {
            return (
                200,
                "application/json",
                json!({
                    "id": "idor-sweep-session",
                    "active": true,
                    "authenticated_at": Utc::now().to_rfc3339(),
                    "identity": {
                        "id": ATTACKER_KRATOS_ID,
                        "traits": {"email": ATTACKER_EMAIL},
                        "verifiable_addresses": [
                            {"value": ATTACKER_EMAIL, "verified": true, "status": "completed"}
                        ]
                    },
                    "authentication_methods": [{"method": "password", "aal": "aal2"}]
                })
                .to_string()
                .into_bytes(),
            );
        }
        return (401, "application/json", Vec::new());
    }
    (404, "application/json", Vec::new())
}

async fn keto_stub(
    method: String,
    target: String,
    _headers: BTreeMap<String, String>,
) -> StubReply {
    if method != "GET" || !target.starts_with("/relation-tuples/check") {
        return (404, "application/json", Vec::new());
    }
    let subject = query_param(&target, "subject_id").unwrap_or_default();
    let object = query_param(&target, "object").unwrap_or_default();
    // Allow exactly the attacker's tenant-A role objects; every other
    // subject or tenant is denied — the membership decision the whole
    // sweep is built on.
    let allowed = subject == ATTACKER.to_string() && object.starts_with(&format!("{TENANT_A}:"));
    (
        200,
        "application/json",
        json!({"allowed": allowed}).to_string().into_bytes(),
    )
}

async fn media_stub(
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
) -> StubReply {
    let path = target.split('?').next().unwrap_or("/");
    let namespace = headers
        .get("x-namespace-id")
        .map(String::as_str)
        .unwrap_or_default();
    let asset_id = path
        .strip_prefix("/v1/assets/")
        .unwrap_or_default()
        .split('/')
        .next()
        .unwrap_or_default();
    // Success **only** for tenant B's own namespace: an attacker request
    // must arrive scoped to tenant A and get a 404. A 2xx to an attacker
    // request would mean the gateway sent the foreign namespace.
    if namespace == TENANT_B.to_string() && asset_id == "idor-asset-b" {
        if method == "DELETE" {
            return (204, "application/json", Vec::new());
        }
        return (
            200,
            "application/json",
            json!({"id": asset_id}).to_string().into_bytes(),
        );
    }
    if namespace == TENANT_A.to_string() {
        return (
            404,
            "application/json",
            json!({"code": "not_found", "detail": "no such asset in namespace"})
                .to_string()
                .into_bytes(),
        );
    }
    (
        500,
        "application/json",
        json!({"code": "idor_stub", "detail": "unexpected namespace"})
            .to_string()
            .into_bytes(),
    )
}

fn query_param(target: &str, name: &str) -> Option<String> {
    let query = target.split_once('?')?.1;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        if percent_decode(key) == name {
            return Some(percent_decode(value));
        }
    }
    None
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[index + 1..index + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    None => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn start_stubs() -> (String, String, String) {
    let kratos = spawn_stub(kratos_stub).await;
    let keto = spawn_stub(keto_stub).await;
    let media = spawn_stub(media_stub).await;
    (kratos, keto, media)
}

// Configuration and assembly
// ---------------------------------------------------------------------------

fn sweep_config(kratos_url: String, keto_url: String, media_url: String) -> AppConfig {
    let mut config = AppConfig::default();
    config.telemetry.service_name = "sanvi-idor-sweep".to_string();
    config.server.rate_limit.enabled = false;
    config.features.tenancy = true;
    config.features.identity = true;
    config.features.platform_admin = true;
    config.features.localization = true;
    config.features.theming = true;
    config.features.domains = true;
    config.features.payments = true;
    config.features.advertising = true;
    config.features.billing = false;
    config.features.privacy = false;
    config.features.notifications = false;
    config.tenancy.base_domain = BASE_DOMAIN.to_string();
    config.ory.kratos_public_url = kratos_url.clone();
    config.ory.kratos_admin_url = kratos_url;
    config.ory.keto_read_url = keto_url.clone();
    config.ory.keto_write_url = keto_url;
    config.advertising.tracking_site_key_signing_secret =
        sanvi_telemetry::Redacted::new("idor-sweep-tracking-secret-0123456789".to_string());
    config.advertising.oauth_state_signing_secret =
        sanvi_telemetry::Redacted::new("idor-sweep-oauth-secret-0123456789ab".to_string());
    // 32 zero bytes, standard base64 — `token_kek_bytes()` wants exactly
    // 32 decoded bytes; the KEK never protects anything real here.
    config.advertising.token_kek =
        sanvi_telemetry::Redacted::new("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string());
    config.media.base_url = media_url;
    config.media.service_api_key = "idor-sweep-media-key".into();
    config
}

/// Advertising's directive gate is faked (allow-everything): the sweep
/// proves *authorization and storage isolation*, not phase 05's consent
/// engine, and the privacy context is deliberately not mounted.
struct AllowAllDirectives;

#[async_trait]
impl DirectiveResolver for AllowAllDirectives {
    async fn resolve(
        &self,
        _subject: &SubjectRef,
        purpose: ProcessingPurpose,
    ) -> Result<PurposeDirective, PrivacyError> {
        Ok(PurposeDirective {
            purpose,
            state: DirectiveState::Allowed,
            source: DirectiveSource::Default,
            jurisdiction: JurisdictionRef::from_static("eu"),
            notice_version: None,
            effective_at: Utc::now(),
            superseded_at: None,
        })
    }

    async fn snapshot(&self, subject: &SubjectRef) -> Result<DirectiveSnapshot, PrivacyError> {
        Ok(DirectiveSnapshot {
            subject: subject.clone(),
            jurisdiction: JurisdictionRef::from_static("eu"),
            directives: vec![],
            minor_opt_in_age: None,
            honours_universal_opt_out: false,
        })
    }
}

async fn seeded_world(pool: &sanvi_persistence::PgPool) -> Seeded {
    // Tenants (platform-level table; the identity RLS suite seeds the
    // same way).
    for (id, slug) in [(TENANT_A, "tenant-a"), (TENANT_B, "tenant-b")] {
        sqlx::query(
            "INSERT INTO tenancy.tenants (id, slug, display_name, status, region, default_locale)
             VALUES ($1, $2, $3, 'active', 'us', 'en')",
        )
        .bind(id)
        .bind(slug)
        .bind(format!("{slug} corp"))
        .execute(pool)
        .await
        .unwrap();
    }

    // The feature gates each context's routes consult (per-tenant), for
    // both tenants. Without these the gated routes deny before their id
    // lookups and sweep 2 would pass trivially on the gate's answer.
    const GATED_FEATURES: &[(&str, &str)] = &[
        ("payments.stripe_connect", "Stripe Connect"),
        ("advertising.google_ads", "Google Ads"),
        ("advertising.meta_ads", "Meta Ads"),
        ("advertising.budget_guardrails", "Budget guardrails"),
        ("access.tenant_entitlements", "Tenant entitlements"),
        ("domains.custom", "Custom domains"),
        ("domains.purchase", "Domain purchase"),
        ("theming.custom_css", "Custom CSS"),
        ("theming.custom_templates", "Custom templates"),
    ];
    let features_manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = features_manager
        .begin_unscoped("seed", "idor-sweep")
        .await
        .unwrap();
    for (key, name) in GATED_FEATURES {
        sqlx::query(
            "INSERT INTO access.features (key, name, kind, default_enabled)
             VALUES ($1, $2, 'boolean', false) ON CONFLICT (key) DO NOTHING",
        )
        .bind(key)
        .bind(name)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .unwrap();
    }
    scope.commit().await.unwrap();
    for tenant in [TENANT_A, TENANT_B] {
        let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
        let mut scope = manager.begin_for(tenant).await.unwrap();
        for (key, _) in GATED_FEATURES {
            sqlx::query(
                "INSERT INTO access.entitlements (tenant_id, feature_key, enabled, source)
                 VALUES ($1, $2, true, 'override') ON CONFLICT DO NOTHING",
            )
            .bind(tenant)
            .bind(key)
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .unwrap();
        }
        scope.commit().await.unwrap();
    }

    // Roles granting every tenant-scope permission the matrix declares,
    // one per tenant, so the Keto stub's membership decision is the only
    // thing separating allow (tenant A) from deny (tenant B).
    let permissions: BTreeSet<String> = matrix()
        .iter()
        .filter(|row| row.is_current_tenant())
        .filter_map(|row| row.declared_permission().map(str::to_string))
        .collect();
    assert!(
        !permissions.is_empty(),
        "the matrix declares tenant permissions"
    );
    for (role_id, tenant) in [(ROLE_A, TENANT_A), (ROLE_B, TENANT_B)] {
        let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
        let mut scope = manager.begin_unscoped("seed", "idor-sweep").await.unwrap();
        let tx = scope.tx_mut().as_inner();
        sqlx::query(
            "INSERT INTO access.roles (id, scope, tenant_id, key, name, is_system)
             VALUES ($1, 'tenant', $2, 'idor_sweep', 'IDOR sweep role', false)",
        )
        .bind(role_id)
        .bind(tenant)
        .execute(&mut **tx)
        .await
        .unwrap();
        for permission in &permissions {
            sqlx::query(
                "INSERT INTO access.role_permissions (role_id, permission) VALUES ($1, $2)",
            )
            .bind(role_id)
            .bind(permission)
            .execute(&mut **tx)
            .await
            .unwrap();
        }
        scope.commit().await.unwrap();
    }

    // The attacker (tenant A member) and tenant B's own member.
    let users_manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = users_manager
        .begin_unscoped("seed", "idor-sweep")
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO identity.users (id, kratos_identity_id, primary_email, email_verified, status)
         VALUES ($1, $2, $3, true, 'active'), ($4, $5, $6, true, 'active')",
    )
    .bind(ATTACKER)
    .bind(uuid::Uuid::parse_str(ATTACKER_KRATOS_ID).unwrap())
    .bind(ATTACKER_EMAIL)
    .bind(MEMBER_B)
    .bind(uuid::Uuid::now_v7())
    .bind(format!("member-{MARKER}@b.example"))
    .execute(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.commit().await.unwrap();
    for (user, tenant, role) in [(ATTACKER, TENANT_A, ROLE_A), (MEMBER_B, TENANT_B, ROLE_B)] {
        let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
        let mut scope = manager.begin_for(tenant).await.unwrap();
        let tx = scope.tx_mut().as_inner();
        sqlx::query(
            "INSERT INTO identity.memberships (tenant_id, user_id, status) VALUES ($1, $2, 'active')",
        )
        .bind(tenant)
        .bind(user)
        .execute(&mut **tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO identity.membership_roles (tenant_id, user_id, role_id) VALUES ($1, $2, $3)",
        )
        .bind(tenant)
        .bind(user)
        .bind(role)
        .execute(&mut **tx)
        .await
        .unwrap();
        scope.commit().await.unwrap();
    }

    // Tenant B's resources. Everything carries the marker somewhere a
    // sloppy handler could echo it.
    let resources = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = resources.begin_for(TENANT_B).await.unwrap();
    let tx = scope.tx_mut().as_inner();
    sqlx::query(
        "INSERT INTO advertising.connections
         (id, tenant_id, platform, external_account_id, currency, timezone, status, token_ref)
         VALUES ($1, $2, 'google_ads', $3, 'JPY', 'Asia/Tokyo', 'active', 'secret-ref')",
    )
    .bind(AD_CONNECTION)
    .bind(TENANT_B)
    .bind(format!("{MARKER}-acct"))
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO advertising.campaigns
         (id, tenant_id, connection_id, platform, name, objective, status, budget)
         VALUES ($1, $2, $3, 'google_ads', $4, 'sales', 'draft', '{}'::jsonb)",
    )
    .bind(AD_CAMPAIGN)
    .bind(TENANT_B)
    .bind(AD_CONNECTION)
    .bind(format!("{MARKER} campaign"))
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO advertising.ad_groups (id, tenant_id, campaign_id, name) VALUES ($1, $2, $3, $4)")
        .bind(AD_GROUP)
        .bind(TENANT_B)
        .bind(AD_CAMPAIGN)
        .bind(format!("{MARKER} ad group"))
        .execute(&mut **tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO advertising.ads (id, tenant_id, ad_group_id, landing_url) VALUES ($1, $2, $3, $4)")
        .bind(AD)
        .bind(TENANT_B)
        .bind(AD_GROUP)
        .bind(format!("https://b.example/{MARKER}"))
        .execute(&mut **tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO advertising.creatives (id, tenant_id, specification) VALUES ($1, $2, $3::jsonb)")
        .bind(CREATIVE)
        .bind(TENANT_B)
        .bind(json!({"name": MARKER}).to_string())
        .execute(&mut **tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO advertising.audiences (id, tenant_id, platform, name, status) VALUES ($1, $2, 'google_ads', $3, 'active')")
        .bind(AUDIENCE)
        .bind(TENANT_B)
        .bind(format!("{MARKER} audience"))
        .execute(&mut **tx)
        .await
        .unwrap();
    let occurred = Utc::now();
    sqlx::query(
        "INSERT INTO advertising.conversion_event_dedup
         (tenant_id, event_id, dedupe_key, occurred_at) VALUES ($1, $2, $2, $3)",
    )
    .bind(TENANT_B)
    .bind(format!("idor-evt-{CONVERSION}"))
    .bind(occurred)
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO advertising.conversion_events
         (id, tenant_id, event_id, dedupe_key, name, occurred_at, consent)
         VALUES ($1, $2, $3, $3, 'purchase', $4, '{}'::jsonb)",
    )
    .bind(CONVERSION)
    .bind(TENANT_B)
    .bind(format!("idor-evt-{CONVERSION}"))
    .bind(occurred)
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO advertising.budget_alerts
         (id, tenant_id, period, period_start, threshold, condition, spend_minor, cap_minor, currency)
         VALUES ($1, $2, 'daily', CURRENT_DATE, 80, 'warn', 1000, 5000, 'JPY')",
    )
    .bind(BUDGET_ALERT)
    .bind(TENANT_B)
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO payments.provider_connections
         (id, tenant_id, provider, external_account_id, status)
         VALUES ($1, $2, 'stripe_connect', $3, 'active')",
    )
    .bind(PAY_CONNECTION)
    .bind(TENANT_B)
    .bind(format!("acct_{MARKER}"))
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO payments.checkouts
         (id, tenant_id, connection_id, reference, amount_minor, currency, status)
         VALUES ($1, $2, $3, $4, 2000, 'JPY', 'open')",
    )
    .bind(CHECKOUT)
    .bind(TENANT_B)
    .bind(PAY_CONNECTION)
    .bind(format!("{MARKER}-order"))
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO payments.payments
         (id, tenant_id, checkout_id, external_payment_id, status, amount_minor, currency)
         VALUES ($1, $2, $3, $4, 'succeeded', 2000, 'JPY')",
    )
    .bind(PAYMENT)
    .bind(TENANT_B)
    .bind(CHECKOUT)
    .bind(format!("pay_{MARKER}"))
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO domains.custom_domains
         (id, tenant_id, hostname, normalized_hostname, kind, role, status)
         VALUES ($1, $2, $3, $3, 'connected', 'primary', 'verified')",
    )
    .bind(DOMAIN)
    .bind(TENANT_B)
    .bind(format!("{MARKER}.b.example"))
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO domains.orders
         (id, tenant_id, hostname, normalized_hostname, term_years, price_minor,
          currency, status, registrant_contact)
         VALUES ($1, $2, $3, $3, 1, 1500, 'USD', 'active', '{}'::jsonb)",
    )
    .bind(ORDER)
    .bind(TENANT_B)
    .bind(format!("{MARKER}-order.example"))
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO identity.invitations (id, tenant_id, email, role_ids, token_hash, expires_at)
         VALUES ($1, $2, $3, ARRAY[$4]::bigint[], $5, now() + interval '7 days')",
    )
    .bind(INVITATION)
    .bind(TENANT_B)
    .bind(format!("invitee-{MARKER}@b.example"))
    .bind(ROLE_B)
    .bind(format!("idor-sweep-invitation-{MARKER}"))
    .execute(&mut **tx)
    .await
    .unwrap();
    scope.commit().await.unwrap();

    Seeded::default()
}

async fn build_sweep_app(
    config: AppConfig,
    pool: sanvi_persistence::PgPool,
) -> actix_web::App<
    impl actix_web::dev::ServiceFactory<
        actix_web::dev::ServiceRequest,
        Config = (),
        Response = actix_web::dev::ServiceResponse<
            tracing_actix_web::StreamSpan<actix_web::body::BoxBody>,
        >,
        Error = actix_web::Error,
        InitError = (),
    > + 'static,
> {
    let metrics = MetricsRegistry::prometheus_only();
    let build_info = BuildInfo::new(
        "0.1.0-test".into(),
        "test".into(),
        "now".into(),
        "test".into(),
        "sanvi-idor-sweep".into(),
    );
    let system = build_system_state(build_info, "test", pool.clone(), None, 1);
    let ids = sanvi_testsupport::test_ids();

    let domains_lookup = build_domains_lookup(pool.clone(), None, &config.domains);
    let custom_domain_lookup: Option<
        Arc<dyn sanvi_tenancy::adapters::resolution::CustomDomainLookup>,
    > = Some(Arc::new(DomainsTenantLookupAdapter::new(
        domains_lookup.clone(),
    )));
    let tenancy = build_tenancy_state(
        pool.clone(),
        ids.clone(),
        None,
        &config.tenancy,
        custom_domain_lookup,
    );

    let localization = Some(build_localization_state(
        pool.clone(),
        ids.clone(),
        &config.localization,
        metrics.prometheus_registry(),
    ));

    let role_catalog: Arc<dyn sanvi_identity::domain::RoleCatalog> =
        Arc::new(sanvi_access::adapters::SqlxRoleCatalog::new(
            sanvi_persistence::TxManager::new(pool.clone(), ids.clone()),
            Arc::new(sanvi_access::adapters::SqlxRoleRepository::new()),
        ));
    let tenant_catalog: Arc<dyn sanvi_identity::domain::TenantCatalog> =
        Arc::new(TenantCatalogAdapter::new(
            sanvi_persistence::TxManager::new(pool.clone(), ids.clone()),
            Arc::new(sanvi_tenancy::adapters::repo::SqlxTenantRepository::new()),
        ));
    let identity = build_identity_state(
        pool.clone(),
        ids.clone(),
        None,
        &config.identity,
        &config.ory.kratos_public_url,
        &config.ory.kratos_admin_url,
        IdentityDependencies {
            role_catalog,
            tenant_catalog,
            notification_localizer: None,
            notification_gateway: None,
        },
    );
    let audit = build_audit_state(
        pool.clone(),
        ids.clone(),
        &config.audit,
        config.features.archival,
    );
    let access = build_access_state(
        pool.clone(),
        ids.clone(),
        &config.access,
        &config.ory.keto_read_url,
        &config.ory.keto_write_url,
        identity.memberships.clone(),
        audit.writer.clone(),
        Arc::new(sanvi_platform_admin::adapters::NoopApprovalGate),
        metrics.prometheus_registry(),
        None,
        std::sync::Arc::new(sanvi_resilience::DegradationRegistry::default()),
    );
    let entitlement_admin: Arc<dyn sanvi_platform_admin::domain::EntitlementAdmin> =
        Arc::new(AccessEntitlementAdmin::new(
            access.grant_entitlement.clone(),
            access.revoke_entitlement.clone(),
            access.list_tenant_overrides.clone(),
        ));
    let platform_admin = build_platform_admin_state(
        pool.clone(),
        ids.clone(),
        &config.platform_admin,
        PlatformAdminDependencies {
            tenants: Arc::new(sanvi_tenancy::adapters::repo::SqlxTenantRepository::new()),
            memberships: identity.memberships.clone(),
            entitlements: Arc::new(sanvi_access::adapters::SqlxEntitlementRepository::new()),
            entitlement_admin,
            audit_writer: audit.writer.clone(),
            privacy_lock: Arc::new(sanvi_platform_admin::adapters::NoopPrivacyLock),
        },
    );
    let theming = Some(build_theming_state(
        pool.clone(),
        ids.clone(),
        &config.theming,
        metrics.prometheus_registry(),
        None,
        access.domain_feature_gate.clone(),
        tenancy.host_resolver.clone(),
    ));
    let translator: Arc<dyn sanvi_domains::domain::GuideTranslator> =
        Arc::new(FallbackGuideTranslator);
    let domains = Some(build_domains_state(
        pool.clone(),
        ids.clone(),
        &config.domains,
        metrics.prometheus_registry(),
        None,
        access.domain_feature_gate.clone(),
        translator,
        domains_lookup,
        tenancy.resolve.clone(),
    ));
    let payments = Some(build_payments_state(
        pool.clone(),
        ids.clone(),
        &config.payments,
        metrics.prometheus_registry(),
        &config.telemetry.environment,
        audit.writer.clone(),
    ));
    let directive_resolver: Arc<dyn DirectiveResolver> = Arc::new(AllowAllDirectives);
    let subprocessors: Arc<dyn sanvi_privacy::domain::SubProcessorRepository> =
        Arc::new(sanvi_privacy::adapters::SqlxSubProcessorRepository::new());
    let advertising = Some(
        build_advertising_state(
            pool.clone(),
            ids.clone(),
            &config.advertising,
            directive_resolver,
            subprocessors,
        )
        .await
        .expect("advertising state is valid"),
    );

    build_app(
        config,
        metrics,
        system,
        tenancy,
        identity,
        access,
        audit,
        platform_admin,
        None, // billing
        None, // privacy
        localization,
        theming,
        domains,
        payments,
        advertising,
        None,                                                                  // accounting
        None,                                                                  // redis
        None,                                                                  // ssr_cache
        std::sync::Arc::new(sanvi_resilience::DegradationRegistry::default()), // degradation
        None, // load_shed: shedding tests build their own stack
    )
}

// ---------------------------------------------------------------------------
// Request plumbing
// ---------------------------------------------------------------------------

async fn call<S, B>(
    app: &S,
    method: &str,
    uri: &str,
    body: Option<Value>,
    host: &str,
) -> (u16, String)
where
    // `aw_test::init_service` hands back a service over the raw
    // `actix_http::Request`, and `TestRequest::to_request()` produces it.
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody + 'static,
    B::Error: std::fmt::Debug,
{
    let builder = aw_test::TestRequest::default()
        .method(actix_web::http::Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri)
        .insert_header((actix_web::http::header::HOST, host))
        .cookie(actix_web::cookie::Cookie::new(
            SESSION_COOKIE,
            SESSION_TOKEN,
        ));
    // Several handlers (including some DELETEs) reject without an
    // idempotency key; the header is inert where it is not read.
    let builder = builder.insert_header((
        "idempotency-key",
        // Payments validates the key as a UUID (NFR-904); a fixed UUID
        // satisfies every handler that only requires presence.
        actix_web::http::header::HeaderValue::from_static("01990f0d-0000-7000-8000-0000000aa001"),
    ));
    let builder = match body {
        Some(body) => builder.set_json(body),
        None => builder,
    };
    let response = aw_test::call_service(app, builder.to_request()).await;
    let status = response.status().as_u16();
    let bytes = actix_web::body::to_bytes(response.into_body())
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn panic_context(method: &str, uri: &str, host: &str, status: u16, body: &str) -> String {
    format!("{method} {uri} (host {host}) answered {status}: {body}")
}

/// Did the seeded tenant-B row a route addresses survive the request? The
/// behavioral half of the destructive-verb assertion.
async fn resource_intact(pattern: &str, seeded: &Seeded, pool: &sanvi_persistence::PgPool) -> bool {
    // The existence probe must run through the tenant scope: the app role
    // is RLS-governed and a bare query has no tenant GUC, so it would
    // report every row missing. `begin_for` is exactly what production
    // queries use.
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = match manager.begin_for(TENANT_B).await {
        Ok(scope) => scope,
        Err(_) => return false,
    };
    let tx = scope.tx_mut().as_inner();
    let (resource_id, query): (i64, &str) = match pattern {
        p if p.starts_with("/api/v1/tenant/ads/connections/") => (
            seeded.ad_connection,
            "SELECT EXISTS(SELECT 1 FROM advertising.connections WHERE tenant_id = $1 AND id = $2)",
        ),
        p if p.starts_with("/api/v1/tenant/ads/creatives/") => (
            seeded.creative,
            "SELECT EXISTS(SELECT 1 FROM advertising.creatives WHERE tenant_id = $1 AND id = $2)",
        ),
        p if p.starts_with("/api/v1/tenant/domains/") => (
            seeded.domain,
            "SELECT EXISTS(SELECT 1 FROM domains.custom_domains WHERE tenant_id = $1 AND id = $2)",
        ),
        p if p.starts_with("/api/v1/tenant/invitations/") => (
            seeded.invitation,
            "SELECT EXISTS(SELECT 1 FROM identity.invitations WHERE tenant_id = $1 AND id = $2)",
        ),
        p if p.starts_with("/api/v1/tenant/members/") => (
            seeded.member_b,
            "SELECT EXISTS(SELECT 1 FROM identity.memberships WHERE tenant_id = $1 AND user_id = $2)",
        ),
        p if p.starts_with("/api/v1/tenant/payments/connections/") => (
            seeded.pay_connection,
            "SELECT EXISTS(SELECT 1 FROM payments.provider_connections WHERE tenant_id = $1 AND id = $2)",
        ),
        p if p.starts_with("/api/v1/tenant/roles/") => (
            ROLE_B,
            "SELECT EXISTS(SELECT 1 FROM access.roles WHERE tenant_id = $1 AND id = $2)",
        ),
        _ => return true,
    };
    sqlx::query_scalar(query)
        .bind(TENANT_B)
        .bind(resource_id)
        .fetch_one(&mut **tx)
        .await
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// The sweeps
// ---------------------------------------------------------------------------

/// Sweep 1: the attacker's tenant-A session, tenant B as the resolved
/// tenant. Permission-gated routes must 403 naming the declared
/// permission; exempt routes must neither 5xx nor echo tenant B's data.
#[actix_web::test]
async fn the_foreign_tenant_host_denies_every_tenant_route() {
    let (kratos, keto, media) = start_stubs().await;
    let config = sweep_config(kratos, keto, media);
    let db = sanvi_testsupport::test_db().await;
    let seeded = seeded_world(&db.app_pool()).await;
    let app = aw_test::init_service(build_sweep_app(config, db.app_pool()).await).await;

    let routes = matrix();
    let rows: Vec<&MatrixRow> = routes
        .iter()
        .filter(|row| row.is_current_tenant())
        .collect();
    assert!(!rows.is_empty(), "the matrix declares CurrentTenant routes");
    for row in rows {
        let uri = format!(
            "{}{}",
            materialize(&row.path, &seeded),
            query_suffix(&row.path)
        );
        let (status, text) = call(
            &app,
            &row.method,
            &uri,
            body_for(&row.method, &row.path),
            HOST_B,
        )
        .await;
        assert_ne!(
            status,
            404,
            "route not mounted: {}",
            panic_context(&row.method, &uri, HOST_B, status, &text)
        );
        assert!(
            !text.contains(MARKER),
            "existence leak: {}",
            panic_context(&row.method, &uri, HOST_B, status, &text)
        );

        // A 403 here is the membership denial: the attacker has a valid
        // session and tenant B is resolvable, so the only remaining deny
        // is the gate. (The body does not name the permission — problem
        // details are localized — but that declaration-to-route binding
        // is the route-matrix gate's static job; this sweep is the
        // runtime-enforcement half.) Routes whose declared permission is
        // `Authenticated`-scope (e.g. claiming an invitation token) are
        // meant to pass the gate for any signed-in subject — their
        // isolation lives in the token or the resource, not the gate —
        // and explicit matrix exemptions run their own checks; all of
        // these must still neither 5xx nor leak.
        let gate_required = row
            .declared_permission()
            .and_then(|permission| {
                sanvi_access::domain::permissions::lookup(permission).map(|def| {
                    !matches!(
                        def.scope,
                        sanvi_access::domain::PermissionScope::Authenticated
                    )
                })
            })
            .unwrap_or(false);
        if gate_required {
            assert_eq!(
                status,
                403,
                "expected the membership denial for {}",
                panic_context(&row.method, &uri, HOST_B, status, &text)
            );
        } else {
            assert_ne!(
                status / 100,
                5,
                "server error under sweep: {}",
                panic_context(&row.method, &uri, HOST_B, status, &text)
            );
        }
    }
}

/// Sweep 2: the attacker is a legitimate admin inside tenant A and asks
/// for tenant B's real resource ids on A's own host. The gate allows;
/// storage scoping must 404 (or 403) and never echo the foreign row. The
/// twin probe with a nonexistent id must answer identically, so a 404
/// reserved for unknown ids cannot double as an existence oracle.
#[actix_web::test]
async fn cross_tenant_resource_ids_stay_invisible_on_the_own_host() {
    let (kratos, keto, media) = start_stubs().await;
    let config = sweep_config(kratos, keto, media);
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let seeded = seeded_world(&pool).await;
    let app = aw_test::init_service(build_sweep_app(config, db.app_pool()).await).await;

    // The control case: the attacker *is* allowed by the gate inside
    // tenant A. Without this the assertions below could pass trivially on
    // the gate's 403, proving nothing about storage scoping.
    let (gate_status, gate_body) =
        call(&app, "GET", "/api/v1/tenant/ads/campaigns", None, HOST_A).await;
    assert_eq!(
        gate_status, 200,
        "the attacker must pass the gate on their own tenant for this sweep to mean anything: {gate_body}"
    );

    let mut swept = 0usize;
    for row in matrix().iter().filter(|row| row.is_current_tenant()) {
        let id_slots: Vec<&str> = slots_of(&row.path)
            .into_iter()
            .filter(|slot| entity_id(&row.path, slot, &seeded).is_some())
            .collect();
        let excluded = SHAPE_A_EXCLUSIONS
            .iter()
            .any(|(method, path, _)| *method == row.method && *path == row.path);
        if excluded || id_slots.is_empty() {
            continue;
        }
        let uri = format!(
            "{}{}",
            materialize(&row.path, &seeded),
            query_suffix(&row.path)
        );
        let (status, text) = call(
            &app,
            &row.method,
            &uri,
            body_for(&row.method, &row.path),
            HOST_A,
        )
        .await;
        assert_ne!(
            status / 100,
            5,
            "server error: {}",
            panic_context(&row.method, &uri, HOST_A, status, &text)
        );
        if status == 204 {
            // Idempotent-delete semantics: `DELETE` answers 204 even when
            // nothing matched, so the isolation proof for a 204 is that
            // the foreign row survived the request.
            assert_eq!(
                row.method,
                "DELETE",
                "a mutating request succeeded cross-tenant: {}",
                panic_context(&row.method, &uri, HOST_A, status, &text)
            );
            assert!(
                resource_intact(&row.path, &seeded, &pool).await,
                "cross-tenant DELETE destroyed a tenant-B row: {}",
                panic_context(&row.method, &uri, HOST_A, status, &text)
            );
        } else {
            assert_ne!(
                status / 100,
                2,
                "cross-tenant request succeeded: {}",
                panic_context(&row.method, &uri, HOST_A, status, &text)
            );
            assert!(
                matches!(status, 403 | 404),
                "expected 403/404: {}",
                panic_context(&row.method, &uri, HOST_A, status, &text)
            );
        }
        assert!(
            !text.contains(MARKER),
            "existence leak: {}",
            panic_context(&row.method, &uri, HOST_A, status, &text)
        );

        // The twin: a nonexistent id must look exactly the same, or the
        // 403/404 above is an existence oracle.
        let twin_uri = materialize(&row.path, &Seeded::unknown_twin());
        let (twin_status, _) = call(
            &app,
            &row.method,
            &twin_uri,
            body_for(&row.method, &row.path),
            HOST_A,
        )
        .await;
        assert_eq!(
            status, twin_status,
            "wrong-tenant and unknown ids answer differently (existence oracle): {} {}",
            row.method, row.path
        );
        swept += 1;
    }
    assert!(swept > 0, "sweep 2 exercised no routes");
}

/// Sweep 3: the tenant-scoped session against every mounted platform
/// route. Billing and privacy are deliberately not mounted here (their
/// platform routes are covered by their own suites) and are excluded by
/// handler context, not by hand-picking rows.
#[actix_web::test]
async fn platform_routes_require_platform_permissions() {
    let (kratos, keto, media) = start_stubs().await;
    let config = sweep_config(kratos, keto, media);
    let db = sanvi_testsupport::test_db().await;
    let seeded = seeded_world(&db.app_pool()).await;
    let app = aw_test::init_service(build_sweep_app(config, db.app_pool()).await).await;

    let unmounted = ["billing", "privacy"];
    let routes = matrix();
    let rows: Vec<&MatrixRow> = routes
        .iter()
        .filter(|row| row.is_platform())
        .filter(|row| !unmounted.contains(&row.context()))
        .collect();
    assert!(!rows.is_empty(), "the matrix declares platform routes");
    for row in rows {
        let uri = format!(
            "{}{}",
            materialize(&row.path, &seeded),
            query_suffix(&row.path)
        );
        let (status, text) = call(
            &app,
            &row.method,
            &uri,
            body_for(&row.method, &row.path),
            HOST_A,
        )
        .await;
        assert!(
            !text.contains(MARKER),
            "existence leak: {}",
            panic_context(&row.method, &uri, HOST_A, status, &text)
        );
        // Same reasoning as sweep 1: a valid session with no platform
        // role must be denied (403), never served.
        if row.declared_permission().is_some() {
            assert_eq!(
                status,
                403,
                "expected the permission denial for {}",
                panic_context(&row.method, &uri, HOST_A, status, &text)
            );
        } else {
            assert_ne!(
                status / 100,
                5,
                "server error: {}",
                panic_context(&row.method, &uri, HOST_A, status, &text)
            );
        }
    }
}

/// Coverage pin: every `CurrentTenant` route in the matrix is exercised
/// by sweep 1 (no exclusions exist there), and sweep 2 covers every
/// id-bearing one except the explicitly-reasoned list below. A route
/// appearing in the matrix whose id slot this suite cannot fill fails
/// here — new phase routes must join the sweep, not silently bypass it.
const SHAPE_A_EXCLUSIONS: &[(&str, &str, &str)] = &[
    (
        "GET",
        "/api/v1/tenant/ads/connections/{platform}/oauth/callback",
        "the {platform} slot is a provider enum; isolation is the signed one-time OAuth state, covered by the advertising contract tests",
    ),
    (
        "POST",
        "/api/v1/tenant/ads/connections/{platform}/oauth/start",
        "the {platform} slot is a provider enum; there is no foreign id to substitute",
    ),
    (
        "DELETE",
        "/api/v1/tenant/theme/assets/{kind}",
        "the {kind} slot is an asset-type enum, not an entity id",
    ),
    (
        "GET",
        "/api/v1/tenant/ecommerce/{tail}",
        "the {tail} slot is a path into ecommerce-service, not a platform entity id; that service isolates its ids by the delegated tenant (forced RLS), covered by its two-tenant HTTP and PostgreSQL tests",
    ),
    (
        "POST",
        "/api/v1/tenant/ecommerce/{tail}",
        "the {tail} slot is a path into ecommerce-service, not a platform entity id; that service isolates its ids by the delegated tenant (forced RLS), covered by its two-tenant HTTP and PostgreSQL tests",
    ),
];

#[test]
fn the_sweep_covers_every_tenant_route_the_matrix_declares() {
    let rows: Vec<MatrixRow> = matrix()
        .into_iter()
        .filter(|row| row.is_current_tenant())
        .collect();
    assert!(
        rows.len() >= 100,
        "the matrix unexpectedly shrank; investigate before trusting this suite"
    );

    let seeded = Seeded::default();
    let mut missing: Vec<String> = Vec::new();
    for row in &rows {
        let slots = slots_of(&row.path);
        if slots.is_empty() {
            continue;
        }
        let has_entity_slot = slots
            .iter()
            .any(|slot| entity_id(&row.path, slot, &seeded).is_some());
        if !has_entity_slot
            && !SHAPE_A_EXCLUSIONS
                .iter()
                .any(|(method, path, _)| *method == row.method && *path == row.path)
        {
            missing.push(format!(
                "{} {} (slots: {})",
                row.method,
                row.path,
                slots.join(", ")
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "id-bearing tenant routes not covered by sweep 2 and not in SHAPE_A_EXCLUSIONS:\n  {}",
        missing.join("\n  ")
    );
    for (method, path, _) in SHAPE_A_EXCLUSIONS {
        assert!(
            rows.iter()
                .any(|row| &row.method == method && &row.path == path),
            "SHAPE_A_EXCLUSIONS names {method} {path}, which the matrix no longer has; prune the list"
        );
    }
}
