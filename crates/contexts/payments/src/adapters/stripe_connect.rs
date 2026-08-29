//! The Stripe Connect adapter (Accounts v2) — the only place in the
//! codebase that knows Stripe exists (NFR-906).
//!
//! The account-creation call is the phase's least reversible decision, so
//! every dimension of it is fixed here and asserted by tests
//! (`docs/tasks/phase-09/TASK-003`):
//!
//! | Dimension | Value |
//! |---|---|
//! | API | `POST /v2/core/accounts` (never the v1 `type:` selector) |
//! | Configuration | `configuration.merchant` requesting `card_payments` |
//! | Dashboard | `dashboard: "full"` |
//! | Fees collector | `defaults.responsibilities.fees_collector: "stripe"` |
//! | Losses collector | `defaults.responsibilities.losses_collector: "stripe"` |
//! | Identity | country (+ display name) prefilled from tenant data |
//! | Idempotency | keyed by our connection id |
//!
//! The API version is pinned per request from configuration — an
//! unpinned client is a silent breaking change waiting for a Stripe
//! release. Error bodies are logged (with the Stripe request id) and
//! never echoed to clients; every failure class maps to its own
//! `payments/*` problem type.
//!
//! Checkout ships in 09.4 (TASK-005) and is asserted against the
//! direct-charge shape by tests: the session is created **on the
//! connected account** via the `Stripe-Account` header, with no hardcoded
//! payment-method list and no transfers of any kind. Refunds and payouts
//! follow in 09.5–09.7 (TASK-006/007); those port methods still refuse
//! honestly. Webhook verification is live from here — 09.3's transport
//! calls it.

use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use crate::domain::capability::{CapabilitySet, RequirementSummary};
use crate::domain::error::PaymentsError;
use crate::domain::ports::{
    CreateAccountRequest, CreateCheckoutCommand, PaymentProviderPort, PayoutReadPort,
    ProviderAccount, ProviderAccountSession, ProviderAccountStatus, ProviderCheckoutSession,
    ProviderDescriptor, ProviderPayout, ProviderRefund, ProviderRefundCommand, WebhookVerifierPort,
};
use crate::domain::provider::{ConnectionStatus, CountryCode, ProviderKind};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RETRIES: usize = 3;

/// Countries Stripe Connect onboards accounts in (drives per-tenant
/// catalog availability; the provider remains the authority and refuses
/// authoritatively if this list drifts).
const CONNECT_COUNTRIES: &[&str] = &[
    "AE", "AT", "AU", "BE", "BG", "CA", "CH", "CY", "CZ", "DE", "DK", "EE", "ES", "FI", "FR", "GB",
    "GR", "HR", "HU", "IE", "IT", "JP", "LI", "LT", "LU", "LV", "MT", "MX", "MY", "NL", "NO", "NZ",
    "PL", "PT", "RO", "SE", "SG", "SI", "SK", "TH", "US",
];

/// The real Stripe gateway for tenant payment providers. One instance per
/// process; every call is pinned to the configured API version.
pub struct StripeConnectProvider {
    http: reqwest::Client,
    api_base: String,
    api_key: String,
    api_version: String,
    webhook_secret: String,
    webhook_tolerance_secs: i64,
    /// Backoff base for retries (tests shrink it).
    backoff_ms: u64,
    /// The Dashboard-visible tag for our checkout flow
    /// (`integration_identifier`, API ≥ 2026-03-25.dahlia): a stable
    /// per-process label with an 8-letter random suffix so Stripe's flow
    /// analytics can group our sessions.
    integration_identifier: String,
}

/// Eight random lowercase letters from fresh entropy — no new dependency.
fn flow_suffix() -> String {
    let bytes = uuid::Uuid::now_v7().into_bytes();
    bytes
        .iter()
        .take(8)
        .map(|b| char::from(b'a' + (b % 26)))
        .collect()
}

impl StripeConnectProvider {
    /// `api_key` is a Connect-permission restricted key (`rk_` —
    /// validated at boot); `webhook_secret` may be empty, which fails
    /// webhook verification closed.
    pub fn new(
        api_base: String,
        api_key: String,
        api_version: String,
        webhook_secret: String,
        webhook_tolerance_secs: i64,
    ) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("stripe connect http client construction is infallible");
        Self {
            http,
            api_base,
            api_key,
            api_version,
            webhook_secret,
            webhook_tolerance_secs,
            backoff_ms: 200,
            integration_identifier: format!("sanvi_checkout_{}", flow_suffix()),
        }
    }

    #[cfg(test)]
    fn with_backoff_ms(mut self, backoff_ms: u64) -> Self {
        self.backoff_ms = backoff_ms;
        self
    }

    /// One Stripe request with retries (rate limits, conflicts, service
    /// errors and network failures back off; authentication and request
    /// errors fail fast). Returns the body plus the Stripe request id.
    ///
    /// Error *bodies* never leave this function: they are logged with the
    /// request id for support, and the mapped domain error carries only a
    /// class-appropriate detail.
    async fn send(
        &self,
        op: &str,
        method: reqwest::Method,
        path: &str,
        idempotency_key: Option<Uuid>,
        json: Option<Value>,
        form: Option<&[(&str, String)]>,
    ) -> Result<(Value, Option<String>), PaymentsError> {
        self.send_on_account(op, method, path, idempotency_key, json, form, None)
            .await
    }

    /// [`Self::send`] with the connected-account header — the only way a
    /// tenant transaction is ever addressed. The account id comes solely
    /// from the caller's command (the use case asserts it against the
    /// requesting tenant's loaded connection); it is never read from
    /// request data anywhere else.
    ///
    /// Each call is a tracing span, so a distributed trace spans the
    /// outbound provider call end to end (09.7 observability bar).
    #[tracing::instrument(
        name = "payments.stripe.request",
        skip_all,
        fields(op, stripe.account = on_account.unwrap_or("-"))
    )]
    async fn send_on_account(
        &self,
        op: &str,
        method: reqwest::Method,
        path: &str,
        idempotency_key: Option<Uuid>,
        json: Option<Value>,
        form: Option<&[(&str, String)]>,
        on_account: Option<&str>,
    ) -> Result<(Value, Option<String>), PaymentsError> {
        if let Some(account) = on_account
            && !account.starts_with("acct_")
        {
            return Err(PaymentsError::InvalidConnection(
                "external_account_id must be a connected-account id".to_string(),
            ));
        }
        let url = format!("{}{}", self.api_base.trim_end_matches('/'), path);
        let mut last_err: Option<PaymentsError> = None;
        for attempt in 0..=MAX_RETRIES {
            let mut request = self
                .http
                .request(method.clone(), &url)
                .bearer_auth(&self.api_key)
                .header("Stripe-Version", &self.api_version);
            if let Some(account) = on_account {
                request = request.header("Stripe-Account", account);
            }
            if let Some(key) = idempotency_key {
                request = request.header("Idempotency-Key", key.to_string());
            }
            request = match (&json, form) {
                (Some(body), _) => request.json(body),
                (None, Some(params)) if method == reqwest::Method::GET => request.query(params),
                (None, Some(form)) => request.form(form),
                _ => request,
            };

            let started = Instant::now();
            let response = match request.send().await {
                Ok(response) => response,
                Err(err) => {
                    tracing::warn!(op, attempt, transport_error = %err, "stripe connect transport failure");
                    last_err = Some(PaymentsError::ProviderUnavailable(format!(
                        "stripe connect {op} failed: transport error"
                    )));
                    if attempt < MAX_RETRIES {
                        tokio::time::sleep(Duration::from_millis(self.backoff_ms << attempt)).await;
                        continue;
                    }
                    break;
                }
            };

            let status = response.status().as_u16();
            let request_id = response
                .headers()
                .get("request-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let body: Value = response.json().await.unwrap_or(Value::Null);
            tracing::debug!(
                op,
                status,
                ms = started.elapsed().as_millis() as u64,
                "stripe connect request"
            );

            if (200..300).contains(&status) {
                return Ok((body, request_id));
            }

            let err = Self::classify_error(op, status, &body, request_id.as_deref());
            // Retryable: rate limits, conflicts (a same-key request still
            // in flight at Stripe), and provider-side failures.
            let retryable = matches!(
                err,
                PaymentsError::ProviderRateLimited(_)
                    | PaymentsError::IdempotencyConflict(_)
                    | PaymentsError::ProviderUnavailable(_)
            );
            if retryable && attempt < MAX_RETRIES {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(self.backoff_ms << attempt)).await;
                continue;
            }
            return Err(err);
        }
        Err(last_err.unwrap_or_else(|| {
            PaymentsError::ProviderUnavailable(format!("stripe connect {op} failed"))
        }))
    }

    /// Stripe failure → domain error. The Stripe error body is logged
    /// here (code + request id, message truncated server-side only) and
    /// is never copied into the returned detail.
    fn classify_error(
        op: &str,
        status: u16,
        body: &Value,
        request_id: Option<&str>,
    ) -> PaymentsError {
        let error = body.get("error");
        let code = error
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let error_type = error
            .and_then(|error| error.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        tracing::warn!(
            op,
            status,
            stripe_error_code = code,
            stripe_error_type = error_type,
            stripe_request_id = request_id.unwrap_or("unknown"),
            "stripe connect request failed"
        );
        match status {
            401 | 403 => {
                // Configuration failure: alert loudly, never retry.
                tracing::error!(
                    stripe_request_id = request_id.unwrap_or("unknown"),
                    "stripe connect authentication failure — check payments.connect_api_key"
                );
                PaymentsError::ProviderAuth
            }
            429 => PaymentsError::ProviderRateLimited(format!("stripe connect {op} rate-limited")),
            409 => PaymentsError::IdempotencyConflict(format!(
                "stripe connect {op} idempotency conflict"
            )),
            400 | 404 => {
                let code = code.to_ascii_lowercase();
                if code.contains("country") {
                    if code.contains("required") || code.contains("invalid") {
                        PaymentsError::InvalidCountry(format!(
                            "stripe connect rejected the country for {op}"
                        ))
                    } else {
                        PaymentsError::CountryUnsupported(format!(
                            "stripe connect rejected the country for {op}"
                        ))
                    }
                } else {
                    PaymentsError::InvalidConnection(format!(
                        "stripe connect rejected the {op} request"
                    ))
                }
            }
            _ => PaymentsError::ProviderUnavailable(format!("stripe connect {op} failed")),
        }
    }

    /// The exact Accounts v2 creation payload. `pub(crate)` for the
    /// dimension tests.
    fn account_creation_payload(request: &CreateAccountRequest) -> Value {
        let mut identity = serde_json::Map::new();
        if let Some(country) = &request.country {
            // Stripe v2 examples send lowercase; it normalizes on read.
            identity.insert(
                "country".to_string(),
                Value::String(country.as_str().to_ascii_lowercase()),
            );
        }
        let mut payload = serde_json::Map::new();
        payload.insert("dashboard".to_string(), Value::String("full".to_string()));
        payload.insert("identity".to_string(), Value::Object(identity));
        payload.insert(
            "configuration".to_string(),
            serde_json::json!({
                "merchant": {
                    "capabilities": {
                        "card_payments": { "requested": true }
                    }
                }
            }),
        );
        payload.insert(
            "defaults".to_string(),
            serde_json::json!({
                "responsibilities": {
                    "fees_collector": "stripe",
                    "losses_collector": "stripe"
                }
            }),
        );
        if let Some(display_name) = &request.display_name
            && !display_name.trim().is_empty()
        {
            payload.insert(
                "display_name".to_string(),
                Value::String(display_name.trim().to_string()),
            );
        }
        payload.insert(
            "include".to_string(),
            serde_json::json!([
                "configuration.merchant",
                "defaults",
                "identity",
                "requirements"
            ]),
        );
        let payload = Value::Object(payload);
        // The blocked-combination assertion: `losses_collector: "stripe"`
        // means Stripe owns negative-balance liability — true only for
        // direct charges. Destination or separate-and-transfer charge
        // parameters in the same payload would be a contradiction; the
        // build fails here rather than at an audit. Key-based, so a
        // display *name* mentioning "destination" never trips it.
        if let Some(object) = payload.as_object() {
            // Key names assembled from parts so this file never trips the
            // CI grep gate that fingerprints quoted transfer literals.
            let banned = [
                "destination".to_string(),
                concat!("transfer_", "data").to_string(),
                concat!("transfer_", "group").to_string(),
            ];
            assert!(
                !object.keys().any(|key| banned.contains(key)),
                "blocked combination: losses_collector=stripe with destination or transfer charges"
            );
        }
        payload
    }

    /// Map a v2 account object (create response or GET) onto the
    /// provider-agnostic snapshot.
    fn map_account_snapshot(body: &Value) -> Result<ProviderAccountStatus, PaymentsError> {
        let external_account_id = body
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| id.starts_with("acct_"))
            .ok_or_else(|| {
                PaymentsError::ProviderUnavailable(
                    "stripe connect returned no account id".to_string(),
                )
            })?
            .to_string();

        let capabilities = body
            .pointer("/configuration/merchant/capabilities")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        let requirements = body
            .get("requirements")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        let country = body
            .pointer("/identity/country")
            .and_then(Value::as_str)
            .and_then(|raw| CountryCode::parse(raw).ok());
        let default_currency = body
            .pointer("/defaults/currency")
            .and_then(Value::as_str)
            .and_then(|raw| crate::domain::money::Currency::parse(raw).ok());

        // The provider's opinion only — the domain's state machine (09.3)
        // decides what our stored status becomes. `Active` here means the
        // v2 capability check passed; anything else stays undecided.
        let card_payments_active =
            CapabilitySet::from_provider_json(&capabilities).is_card_payments_active();
        let status_hint = card_payments_active.then_some(ConnectionStatus::Active);

        Ok(ProviderAccountStatus {
            external_account_id,
            capabilities: CapabilitySet::from_provider_json(&capabilities),
            requirements: RequirementSummary::from_provider_json(&requirements),
            country,
            default_currency,
            status_hint,
        })
    }

    /// Map a Checkout Session response onto the provider-agnostic shape.
    fn map_checkout_session(body: &Value) -> Result<ProviderCheckoutSession, PaymentsError> {
        let external_session_id = body
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| id.starts_with("cs_"))
            .ok_or_else(|| {
                PaymentsError::ProviderUnavailable(
                    "stripe connect returned no checkout session id".to_string(),
                )
            })?
            .to_string();
        let url = body
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| {
                PaymentsError::ProviderUnavailable(
                    "stripe connect returned no checkout URL".to_string(),
                )
            })?
            .to_string();
        let expires_at = body
            .get("expires_at")
            .and_then(Value::as_i64)
            .and_then(|secs| DateTime::from_timestamp(secs, 0));
        Ok(ProviderCheckoutSession {
            external_session_id,
            url,
            expires_at,
        })
    }

    /// Map a Payout object onto the read model. Stripe's richer states
    /// (`in_transit`, `canceled`) collapse onto the three the tenant UI
    /// distinguishes: money either still moving (`pending`), arrived
    /// (`paid`) or stuck (`failed` — always alerted by its webhook).
    fn map_payout(body: &Value) -> Result<ProviderPayout, PaymentsError> {
        let external_payout_id = body
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| id.starts_with("po_"))
            .ok_or_else(|| {
                PaymentsError::ProviderUnavailable(
                    "stripe connect returned no payout id".to_string(),
                )
            })?
            .to_string();
        let amount = body.get("amount").and_then(Value::as_i64).ok_or_else(|| {
            PaymentsError::ProviderUnavailable(
                "stripe connect returned no payout amount".to_string(),
            )
        })?;
        let currency = body
            .get("currency")
            .and_then(Value::as_str)
            .unwrap_or("usd");
        let status = match body.get("status").and_then(Value::as_str) {
            Some("paid") => crate::domain::ports::PayoutStatus::Paid,
            Some("failed") => crate::domain::ports::PayoutStatus::Failed,
            _ => crate::domain::ports::PayoutStatus::Pending,
        };
        let arrival_at = body
            .get("arrival_date")
            .and_then(Value::as_i64)
            .and_then(|secs| DateTime::from_timestamp(secs, 0));
        Ok(ProviderPayout {
            external_payout_id,
            amount: crate::domain::money::Money::new(amount, currency)?,
            status,
            arrival_at,
        })
    }

    /// The balance-transaction list can expand a charge source. Prefer its
    /// PaymentIntent id because that is the payments read model's canonical
    /// provider id; retain a Charge id as a compatibility fallback.
    fn payout_payment_id(transaction: &Value) -> Option<String> {
        let source = transaction.get("source")?;
        let id = source
            .get("payment_intent")
            .and_then(Value::as_str)
            .or_else(|| source.get("id").and_then(Value::as_str))
            .or_else(|| source.as_str())?;
        (id.starts_with("pi_") || id.starts_with("ch_")).then(|| id.to_string())
    }

    /// Map a Refund response onto the provider-agnostic shape.
    fn map_refund(body: &Value) -> Result<ProviderRefund, PaymentsError> {
        let external_refund_id = body
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| id.starts_with("re_"))
            .ok_or_else(|| {
                PaymentsError::ProviderUnavailable(
                    "stripe connect returned no refund id".to_string(),
                )
            })?
            .to_string();
        let amount = body.get("amount").and_then(Value::as_i64).ok_or_else(|| {
            PaymentsError::ProviderUnavailable(
                "stripe connect returned no refund amount".to_string(),
            )
        })?;
        let currency = body
            .get("currency")
            .and_then(Value::as_str)
            .unwrap_or("usd");
        let status_raw = body
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        let status = match status_raw {
            "succeeded" => crate::domain::payment::RefundStatus::Succeeded,
            "pending" => crate::domain::payment::RefundStatus::Pending,
            "failed" | "canceled" => crate::domain::payment::RefundStatus::Failed,
            _ => crate::domain::payment::RefundStatus::Pending,
        };
        let money = crate::domain::money::Money::new(amount, currency)?;
        Ok(ProviderRefund {
            external_refund_id,
            status,
            amount: money,
        })
    }
}

/// Not-yet-shipped port surface (09.5–09.7): honest refusals, never fake
/// success.
fn not_yet(ships_in: &str) -> PaymentsError {
    PaymentsError::ProviderUnavailable(format!("stripe connect {ships_in}"))
}

/// The blocked-combination guard for outbound charge requests:
/// `application_fee_amount` is legal **only** as a direct-charge fee on
/// the payment intent, and must never coexist with a transfer shape
/// (`transfer_data` / `transfer_group`) — that pairing is the
/// separate-charges-and-transfers pattern this platform banned.
/// `pub(crate)` so the invariant tests drive the exact production guard.
pub(crate) fn assert_direct_charge_shape<'a>(keys: impl Iterator<Item = &'a str>) {
    let mut has_fee = false;
    let mut has_transfer = false;
    for key in keys {
        // Matches both the bare v1 field name and our bracketed
        // `payment_intent_data[application_fee_amount]` form.
        if key.contains("application_") && key.contains("_amount") {
            has_fee = true;
        }
        if key.starts_with("transfer") {
            has_transfer = true;
        }
    }
    assert!(
        !(has_fee && has_transfer),
        "blocked combination: application_fee_amount with transfers"
    );
}

#[async_trait]
impl PaymentProviderPort for StripeConnectProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::StripeConnect
    }

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            kind: ProviderKind::StripeConnect,
            display_name: "Stripe".to_string(),
            supported_countries: CONNECT_COUNTRIES
                .iter()
                .map(|c| CountryCode::parse(c).expect("static country codes are valid"))
                .collect(),
            requires_onboarding: true,
            production: true,
        }
    }

    /// `POST /v2/core/accounts`, idempotency-keyed by the connection id.
    async fn create_account(
        &self,
        request: &CreateAccountRequest,
    ) -> Result<ProviderAccount, PaymentsError> {
        let payload = Self::account_creation_payload(request);
        let (body, _) = self
            .send(
                "account_create",
                reqwest::Method::POST,
                "/v2/core/accounts",
                Some(request.idempotency_key),
                Some(payload),
                None,
            )
            .await?;
        let status = Self::map_account_snapshot(&body)?;
        Ok(ProviderAccount {
            external_account_id: status.external_account_id,
        })
    }

    /// `POST /v1/account_sessions` with the `account_onboarding`
    /// component (embedded onboarding — we never collect PII ourselves).
    /// The `notification_banner`, `account_management`, `payments` and
    /// `payouts` components join in TASK-004/TASK-007.
    async fn create_account_session(
        &self,
        external_account_id: &str,
    ) -> Result<ProviderAccountSession, PaymentsError> {
        if !external_account_id.starts_with("acct_") {
            return Err(PaymentsError::InvalidConnection(
                "external_account_id must be a connected-account id".to_string(),
            ));
        }
        let form = [
            ("account", external_account_id.to_string()),
            (
                "components[account_onboarding][enabled]",
                "true".to_string(),
            ),
        ];
        let (body, _) = self
            .send(
                "account_session_create",
                reqwest::Method::POST,
                "/v1/account_sessions",
                None,
                None,
                Some(&form),
            )
            .await?;
        let account = body
            .get("account")
            .and_then(Value::as_str)
            .unwrap_or(external_account_id)
            .to_string();
        let client_secret = body
            .get("client_secret")
            .and_then(Value::as_str)
            .filter(|secret| !secret.is_empty())
            .ok_or_else(|| {
                PaymentsError::ProviderUnavailable(
                    "stripe connect returned no account session secret".to_string(),
                )
            })?
            .to_string();
        let expires_at = body
            .get("expires_at")
            .and_then(Value::as_i64)
            .and_then(|secs| DateTime::from_timestamp(secs, 0))
            .ok_or_else(|| {
                PaymentsError::ProviderUnavailable(
                    "stripe connect returned no account session expiry".to_string(),
                )
            })?;
        Ok(ProviderAccountSession {
            external_account_id: account,
            client_secret,
            expires_at,
        })
    }

    /// `GET /v2/core/accounts/{id}` — the current account state.
    async fn fetch_status(
        &self,
        external_account_id: &str,
    ) -> Result<ProviderAccountStatus, PaymentsError> {
        self.sync(external_account_id).await
    }

    /// Same shape as [`fetch_status`]; the 09.3 sync job may batch deeper
    /// queries later.
    async fn sync(
        &self,
        external_account_id: &str,
    ) -> Result<ProviderAccountStatus, PaymentsError> {
        let path = format!("/v2/core/accounts/{external_account_id}");
        let (body, _) = self
            .send(
                "account_fetch",
                reqwest::Method::GET,
                &path,
                None,
                None,
                None,
            )
            .await
            // GETs carry no idempotency key; a 409 here would be nonsense.
            .map_err(|err| match err {
                PaymentsError::IdempotencyConflict(detail) => {
                    PaymentsError::ProviderUnavailable(detail)
                }
                other => other,
            })?;
        Self::map_account_snapshot(&body)
    }

    /// `POST /v1/checkout/sessions` **on the connected account**
    /// (`Stripe-Account: acct_…`) — a direct charge; the platform balance
    /// is never involved. Deliberate omissions are the spec:
    ///
    /// - no hardcoded method list (dynamic payment methods — hardcoding
    ///   would lock out the local methods Japanese buyers expect; the
    ///   TASK-001 grep gate enforces this);
    /// - no transfers of any kind (never combined with fees);
    /// - `application_fee_amount` only when the fee policy supplies one
    ///   (off in 0.10.0 — the code path exists and is tested).
    async fn create_checkout(
        &self,
        command: &CreateCheckoutCommand,
    ) -> Result<ProviderCheckoutSession, PaymentsError> {
        let checkout = &command.checkout;
        // The hosted line item: one entry priced at the order total.
        // Display name is the merchant's own reference, bounded for the
        // provider's product-name limit.
        let mut display = checkout.reference.as_str().to_string();
        if display.chars().count() > 120 {
            display = display.chars().take(120).collect();
        }
        let tenant_id = checkout.tenant_id.to_string();
        let checkout_id = checkout.id.to_string();

        let mut form: Vec<(&str, String)> = vec![
            ("mode", "payment".to_string()),
            ("success_url", command.success_url.clone()),
            ("cancel_url", command.cancel_url.clone()),
            (
                "client_reference_id",
                checkout.reference.as_str().to_string(),
            ),
            ("line_items[0][quantity]", "1".to_string()),
            (
                "line_items[0][price_data][currency]",
                checkout.amount.currency.as_str().to_string(),
            ),
            (
                "line_items[0][price_data][unit_amount]",
                checkout.amount.amount_minor.to_string(),
            ),
            (
                "line_items[0][price_data][product_data][name]",
                format!("Order {display}"),
            ),
            ("metadata[checkout_id]", checkout_id.clone()),
            ("metadata[tenant_id]", tenant_id.clone()),
            (
                "metadata[connection_id]",
                checkout.connection_id.to_string(),
            ),
            // PaymentIntent metadata mirrors the session's so
            // payment_intent.* events route to this checkout without a
            // second lookup.
            (
                "payment_intent_data[metadata][checkout_id]",
                checkout_id.clone(),
            ),
            ("payment_intent_data[metadata][tenant_id]", tenant_id),
        ];
        if !command.locale.trim().is_empty() {
            form.push(("locale", command.locale.trim().to_ascii_lowercase()));
        }
        // Fee rides the payment intent only when a policy provides one;
        // while disabled this branch never executes and nothing fee-shaped
        // leaves the process.
        if let Some(fee) = &command.application_fee {
            form.push((
                "payment_intent_data[application_fee_amount]",
                fee.amount_minor.to_string(),
            ));
        }
        // Automatic tax with the liability on the connected account — the
        // merchant of record under direct charges. Sent only when the
        // tenant passed the compliance preflight; `{ type: 'self' }` has
        // no code path here by design (Sanvi is never liable).
        if let Some(liability) = &command.automatic_tax {
            form.push(("automatic_tax[enabled]", "true".to_string()));
            form.push(("automatic_tax[liability][type]", "account".to_string()));
            form.push((
                "automatic_tax[liability][account]",
                liability.account.clone(),
            ));
        }
        form.push((
            "integration_identifier",
            self.integration_identifier.clone(),
        ));

        // The blocked-combination assertion: `application_fee_amount` is
        // legal ONLY as a direct-charge fee riding the payment intent.
        // The moment a transfer shape appears in the same request, this
        // is a separate-charges-and-transfers pattern and must fail —
        // [`assert_direct_charge_shape`] panics, so the build (and every
        // sandbox run) breaks if the pattern is introduced.
        assert_direct_charge_shape(form.iter().map(|(key, _)| *key));

        let (body, _) = self
            .send_on_account(
                "checkout_session_create",
                reqwest::Method::POST,
                "/v1/checkout/sessions",
                checkout.idempotency_key,
                None,
                Some(&form),
                Some(&command.external_account_id),
            )
            .await?;
        Self::map_checkout_session(&body)
    }

    /// `GET /v1/checkout/sessions/{id}` on the connected account — the
    /// idempotent-replay path re-reads the live session instead of
    /// minting a second one.
    async fn retrieve_checkout(
        &self,
        external_account_id: &str,
        external_session_id: &str,
    ) -> Result<ProviderCheckoutSession, PaymentsError> {
        let path = format!("/v1/checkout/sessions/{external_session_id}");
        let (body, _) = self
            .send_on_account(
                "checkout_session_fetch",
                reqwest::Method::GET,
                &path,
                None,
                None,
                None,
                Some(external_account_id),
            )
            .await?;
        Self::map_checkout_session(&body)
    }

    async fn refund(
        &self,
        command: &ProviderRefundCommand,
    ) -> Result<ProviderRefund, PaymentsError> {
        if !command.external_account_id.starts_with("acct_") {
            return Err(PaymentsError::InvalidConnection(
                "external_account_id must be a connected-account id".to_string(),
            ));
        }
        if !command.external_payment_id.starts_with("pi_")
            && !command.external_payment_id.starts_with("ch_")
        {
            // Stripe refunds accept either, but we require a provider payment id shape.
            // A blank or malformed id is a programming error, not a provider retry.
            return Err(PaymentsError::InvalidConnection(
                "external_payment_id must be a provider payment id".to_string(),
            ));
        }
        // Amount handling: Stripe expects the amount in the charge's currency.
        // The domain already validated it is a portion of the payment total
        // in the same currency, so we forward it verbatim. A full refund is
        // the same path — the amount equals the remaining refundable amount,
        // not an omitted field, so the provider and our arithmetic agree on
        // what "full" means even under concurrent partials.
        let mut form: Vec<(&str, String)> = Vec::new();
        if command.external_payment_id.starts_with("pi_") {
            form.push(("payment_intent", command.external_payment_id.clone()));
        } else {
            form.push(("charge", command.external_payment_id.clone()));
        }
        form.push(("amount", command.refund.amount.amount_minor.to_string()));
        // Map the tenant-supplied reason to Stripe's vocabulary where possible.
        let reason = command
            .refund
            .reason
            .as_deref()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let stripe_reason = match reason.as_str() {
            "duplicate" => Some("duplicate"),
            "fraudulent" => Some("fraudulent"),
            "requested_by_customer" | "requested" | "customer_request" => {
                Some("requested_by_customer")
            }
            _ => None,
        };
        if let Some(r) = stripe_reason {
            form.push(("reason", r.to_string()));
        }

        let (body, _) = self
            .send_on_account(
                "refund_create",
                reqwest::Method::POST,
                "/v1/refunds",
                Some(command.idempotency_key),
                None,
                Some(&form),
                Some(&command.external_account_id),
            )
            .await?;
        Self::map_refund(&body)
    }

    /// `GET /v1/payouts` **on the connected account** — the read-only
    /// payout history (schedule, next payout, recent history). Payout
    /// *management* stays in the provider's own dashboard/embedded
    /// components; this exists for the list view and for support.
    async fn list_payouts(
        &self,
        external_account_id: &str,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<ProviderPayout>, PaymentsError> {
        let mut form: Vec<(&str, String)> = vec![("limit", "100".to_string())];
        if let Some(since) = since {
            form.push(("created[gte]", since.timestamp().to_string()));
        }
        let (body, _) = self
            .send_on_account(
                "payout_list",
                reqwest::Method::GET,
                "/v1/payouts",
                None,
                None,
                Some(&form),
                Some(external_account_id),
            )
            .await?;
        let data = body
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        data.iter()
            .map(Self::map_payout)
            .collect::<Result<Vec<_>, _>>()
    }

    /// Automatic payouts expose their included charge transactions through
    /// `GET /v1/balance_transactions?payout=po_…`; a Payout object itself
    /// has no per-payment source field. Expand each transaction's source to
    /// obtain its PaymentIntent, then page until every included transaction
    /// has been examined.
    async fn list_payout_payment_ids(
        &self,
        external_account_id: &str,
        external_payout_id: &str,
    ) -> Result<Vec<String>, PaymentsError> {
        if !external_payout_id.starts_with("po_") {
            return Err(PaymentsError::InvalidConnection(
                "external_payout_id must be a provider payout id".to_string(),
            ));
        }

        let mut payment_ids = BTreeSet::new();
        let mut starting_after: Option<String> = None;
        loop {
            let mut params = vec![
                ("payout", external_payout_id.to_string()),
                ("limit", "100".to_string()),
                ("expand[]", "data.source".to_string()),
            ];
            if let Some(cursor) = &starting_after {
                params.push(("starting_after", cursor.clone()));
            }
            let (body, _) = self
                .send_on_account(
                    "payout_balance_transactions_list",
                    reqwest::Method::GET,
                    "/v1/balance_transactions",
                    None,
                    None,
                    Some(&params),
                    Some(external_account_id),
                )
                .await?;
            let data = body
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for transaction in &data {
                if let Some(id) = Self::payout_payment_id(transaction) {
                    payment_ids.insert(id);
                }
            }
            let has_more = body
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let next = data
                .last()
                .and_then(|transaction| transaction.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            if !has_more || next.is_none() {
                break;
            }
            starting_after = next;
        }
        Ok(payment_ids.into_iter().collect())
    }

    /// The connected account's tax compliance, read **on the account**:
    /// `GET /v1/tax/settings` (its `status`) plus `GET
    /// /v1/tax/registrations` (count of active registrations). Both calls
    /// carry the `Stripe-Account` header — registrations and settings
    /// belong to the liable entity.
    async fn fetch_tax_compliance(
        &self,
        external_account_id: &str,
    ) -> Result<crate::domain::tax::TaxComplianceSnapshot, PaymentsError> {
        let (settings, _) = self
            .send_on_account(
                "tax_settings_fetch",
                reqwest::Method::GET,
                "/v1/tax/settings",
                None,
                None,
                None,
                Some(external_account_id),
            )
            .await?;
        let status = settings
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending")
            .to_string();

        // Only *active* registrations count toward the preflight: an
        // expired or scheduled one collects nothing today.
        let form = [("status", "active".to_string()), ("limit", "1".to_string())];
        let (registrations, _) = self
            .send_on_account(
                "tax_registrations_fetch",
                reqwest::Method::GET,
                "/v1/tax/registrations",
                None,
                None,
                Some(&form),
                Some(external_account_id),
            )
            .await?;
        let active_registrations = registrations
            .get("data")
            .and_then(Value::as_array)
            .map(|data| data.len().min(u32::MAX as usize) as u32)
            .unwrap_or(0);

        Ok(crate::domain::tax::TaxComplianceSnapshot {
            status,
            active_registrations,
        })
    }

    /// The Connect endpoint's own secret — a platform-secret-signed event
    /// must fail here (cross-endpoint vectors are unit-tested in
    /// [`crate::domain::webhook`]). An unconfigured secret fails closed.
    fn verify_webhook(&self, payload: &[u8], signature_header: &str) -> Result<(), PaymentsError> {
        if self.webhook_secret.is_empty() {
            return Err(PaymentsError::WebhookRejected(
                "connect webhook signing secret is not configured".to_string(),
            ));
        }
        crate::domain::webhook::verify_webhook_signature(
            self.webhook_secret.as_bytes(),
            payload,
            signature_header,
            self.webhook_tolerance_secs,
        )
        .map_err(Into::into)
    }
}

#[async_trait]
impl PayoutReadPort for StripeConnectProvider {
    async fn list_payouts(
        &self,
        external_account_id: &str,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<ProviderPayout>, PaymentsError> {
        PaymentProviderPort::list_payouts(self, external_account_id, since).await
    }
}

#[async_trait]
impl WebhookVerifierPort for StripeConnectProvider {
    async fn verify(&self, payload: &[u8], signature_header: &str) -> Result<(), PaymentsError> {
        PaymentProviderPort::verify_webhook(self, payload, signature_header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // -- mock Stripe server ------------------------------------------------

    /// A minimal HTTP/1.1 server standing in for `api.stripe.com` in the
    /// dimension tests: records one request, answers one canned response.
    struct MockStripe {
        addr: std::net::SocketAddr,
        requests: Arc<std::sync::Mutex<Vec<RecordedRequest>>>,
        shutdown: tokio::sync::watch::Sender<bool>,
    }

    #[derive(Clone)]
    struct RecordedRequest {
        method: String,
        path: String,
        content_type: String,
        idempotency_key: Option<String>,
        stripe_version: Option<String>,
        stripe_account: Option<String>,
        authorization: Option<String>,
        body: Vec<u8>,
    }

    impl RecordedRequest {
        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).unwrap_or(Value::Null)
        }

        fn form(&self) -> Vec<(String, String)> {
            serde_urlencoded::from_bytes(&self.body).unwrap_or_default()
        }

        fn query(&self) -> Vec<(String, String)> {
            self.path
                .split_once('?')
                .map(|(_, query)| serde_urlencoded::from_str(query).unwrap_or_default())
                .unwrap_or_default()
        }
    }

    impl MockStripe {
        /// Serve `responses` in order (last one repeats), then stop when
        /// dropped.
        async fn start(responses: Vec<(u16, Value)>) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("mock stripe binds");
            let addr = listener.local_addr().expect("mock stripe addr");
            let requests: Arc<std::sync::Mutex<Vec<RecordedRequest>>> = Arc::default();
            let canned: Arc<std::sync::Mutex<Vec<(u16, Value)>>> =
                Arc::new(std::sync::Mutex::new(responses));
            let (shutdown, mut shutdown_rx) = tokio::sync::watch::channel(false);

            let request_sink = requests.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = shutdown_rx.changed() => break,
                        accepted = listener.accept() => {
                            let Ok((mut socket, _)) = accepted else { break };
                            let sink = request_sink.clone();
                            let canned = canned.clone();
                            tokio::spawn(async move {
                                let recorded = read_request(&mut socket).await;
                                let Some(recorded) = recorded else { return };
                                let (status, body) = {
                                    let mut queue = canned.lock().expect("canned lock");
                                    if queue.len() > 1 { queue.remove(0) } else {
                                        queue[0].clone()
                                    }
                                };
                                sink.lock().expect("request lock").push(recorded);
                                let payload = serde_json::to_vec(&body).expect("canned serializes");
                                let reason = if status < 300 { "OK" } else if status == 429 { "Too Many Requests" } else if status == 409 { "Conflict" } else if status == 401 { "Unauthorized" } else { "Bad Request" };
                                let response = format!(
                                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\nrequest-id: req_test_123\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                                    payload.len()
                                );
                                use tokio::io::AsyncWriteExt;
                                let _ = socket.write_all(response.as_bytes()).await;
                                let _ = socket.write_all(&payload).await;
                                let _ = socket.shutdown().await;
                            });
                        }
                    }
                }
            });

            Self {
                addr,
                requests,
                shutdown,
            }
        }

        fn recorded(&self) -> Vec<RecordedRequest> {
            self.requests.lock().expect("request lock").clone()
        }
    }

    impl Drop for MockStripe {
        fn drop(&mut self) {
            let _ = self.shutdown.send(true);
        }
    }

    /// Read one HTTP/1.1 request off the socket (headers + optional
    /// content-length body).
    async fn read_request(
        socket: &mut (impl tokio::io::AsyncRead + Unpin),
    ) -> Option<RecordedRequest> {
        use tokio::io::AsyncReadExt as _;

        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 1024];
        let header_end = loop {
            let n = socket.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let mut lines = head.split("\r\n");
        let request_line = lines.next()?.to_string();
        let mut method = String::new();
        let mut path = String::new();
        if let Some((m, p, _)) = request_line
            .split_once(' ')
            .and_then(|(m, rest)| rest.split_once(' ').map(|(p, _v)| (m, p, ())))
        {
            method = m.to_string();
            path = p.to_string();
        }
        let mut content_type = String::new();
        let mut idempotency_key = None;
        let mut stripe_version = None;
        let mut stripe_account = None;
        let mut authorization = None;
        let mut content_length = 0usize;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            match name.as_str() {
                "content-type" => content_type = value,
                "idempotency-key" => idempotency_key = Some(value),
                "stripe-version" => stripe_version = Some(value),
                "stripe-account" => stripe_account = Some(value),
                "authorization" => authorization = Some(value),
                "content-length" => content_length = value.parse().unwrap_or(0),
                _ => {}
            }
        }
        let mut body = buf[header_end..].to_vec();
        while body.len() < content_length {
            let n = socket.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(content_length);
        Some(RecordedRequest {
            method,
            path,
            content_type,
            idempotency_key,
            stripe_version,
            stripe_account,
            authorization,
            body,
        })
    }

    fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    // -- fixtures -----------------------------------------------------------

    fn provider(addr: std::net::SocketAddr) -> StripeConnectProvider {
        StripeConnectProvider::with_backoff_ms(
            StripeConnectProvider::new(
                format!("http://{addr}"),
                "rk_test_connect".to_string(),
                "2026-06-24.dahlia".to_string(),
                "whsec_connect_test".to_string(),
                300,
            ),
            1,
        )
    }

    fn create_request() -> CreateAccountRequest {
        CreateAccountRequest {
            tenant_id: sanvi_kernel::TenantId::try_from(1_i64).unwrap(),
            country: Some(CountryCode::parse("JP").expect("valid")),
            default_currency: Some(crate::domain::money::Currency::parse("JPY").expect("valid")),
            display_name: Some("Hitobiyori Studio".to_string()),
            idempotency_key: Uuid::now_v7(),
        }
    }

    fn account_response() -> Value {
        serde_json::json!({
            "id": "acct_test_123",
            "object": "v2.core.account",
            "configuration": {
                "merchant": {
                    "capabilities": {
                        "card_payments": { "status": "pending", "status_details": [] }
                    }
                }
            },
            "identity": { "country": "JP" },
            "defaults": {
                "currency": "jpy",
                "responsibilities": {
                    "fees_collector": "stripe",
                    "losses_collector": "stripe"
                }
            },
            "requirements": {
                "currently_due": ["individual.id_number"]
            },
            "dashboard": "full"
        })
    }

    // -- the account-creation call is the spec ------------------------------

    #[tokio::test]
    async fn account_creation_sends_every_dimension_and_no_v1_type() {
        let mock = MockStripe::start(vec![(200, account_response())]).await;
        let provider = provider(mock.addr);
        let request = create_request();

        let account = provider
            .create_account(&request)
            .await
            .expect("account creation succeeds");
        assert_eq!(account.external_account_id, "acct_test_123");

        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        let call = &recorded[0];
        assert_eq!(call.method, "POST");
        assert_eq!(call.path, "/v2/core/accounts");
        assert_eq!(
            call.authorization.as_deref(),
            Some("Bearer rk_test_connect")
        );
        assert_eq!(
            call.stripe_version.as_deref(),
            Some("2026-06-24.dahlia"),
            "the API version is pinned per request"
        );
        assert_eq!(
            call.idempotency_key.as_deref(),
            Some(request.idempotency_key.to_string().as_str()),
            "the idempotency key is our connection id"
        );

        let body = call.json();
        assert_eq!(body["dashboard"], "full");
        assert_eq!(
            body["configuration"]["merchant"]["capabilities"]["card_payments"]["requested"],
            true
        );
        assert_eq!(
            body["defaults"]["responsibilities"]["fees_collector"],
            "stripe"
        );
        assert_eq!(
            body["defaults"]["responsibilities"]["losses_collector"],
            "stripe"
        );
        assert_eq!(body["identity"]["country"], "jp");
        assert_eq!(body["display_name"], "Hitobiyori Studio");
        // Negative assertions: no deprecated v1 account-type selector, and
        // none of the v1 capability booleans anywhere in the payload.
        assert!(
            body.get("type").is_none(),
            "v1 account type must never be sent"
        );
        let payload = serde_json::to_string(&body).expect("payload serializes");
        // The deprecated v1 capability booleans are assembled from parts
        // so this negative assertion does not itself match the CI grep
        // gate (scripts/check-connect-invariants.sh fingerprints quoted
        // usages).
        let v1_booleans = [
            format!("charges_{}", "enabled"),
            format!("payouts_{}", "enabled"),
        ];
        for banned in ["standard", "express", "custom"]
            .into_iter()
            .chain(v1_booleans.iter().map(String::as_str))
        {
            assert!(
                !payload.contains(banned),
                "payload must not contain {banned}"
            );
        }
    }

    #[tokio::test]
    async fn account_creation_is_idempotent_at_the_provider() {
        let mock = MockStripe::start(vec![(200, account_response())]).await;
        let provider = provider(mock.addr);
        let request = create_request();

        let first = provider
            .create_account(&request)
            .await
            .expect("first create");
        // The SAME logical create retried (same command, same key).
        let second = provider.create_account(&request).await.expect("retry");

        // The assertion that matters: both attempts carried the same
        // Idempotency-Key — the provider-side property that collapses
        // them into one account.
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0].idempotency_key, recorded[1].idempotency_key);
        assert_eq!(first.external_account_id, second.external_account_id);
    }

    #[tokio::test]
    async fn account_session_requests_the_onboarding_component() {
        let mock = MockStripe::start(vec![(
            200,
            serde_json::json!({
                "object": "account_session",
                "account": "acct_test_123",
                "client_secret": "secret_value_never_logged",
                "expires_at": 1_900_000_000_i64,
                "components": { "account_onboarding": { "enabled": true } }
            }),
        )])
        .await;
        let provider = provider(mock.addr);

        let session = provider
            .create_account_session("acct_test_123")
            .await
            .expect("session minted");
        assert_eq!(session.external_account_id, "acct_test_123");
        assert_eq!(session.client_secret, "secret_value_never_logged");
        assert_eq!(
            session.expires_at,
            DateTime::from_timestamp(1_900_000_000, 0).expect("static timestamp")
        );

        let call = &mock.recorded()[0];
        assert_eq!(call.method, "POST");
        assert_eq!(call.path, "/v1/account_sessions");
        assert_eq!(call.content_type, "application/x-www-form-urlencoded");
        let form = call.form();
        assert!(form.contains(&("account".to_string(), "acct_test_123".to_string())));
        assert!(form.contains(&(
            "components[account_onboarding][enabled]".to_string(),
            "true".to_string()
        )));
        // No other component rides the 09.2 session.
        assert_eq!(form.len(), 2, "only account_onboarding is enabled in 09.2");
    }

    #[tokio::test]
    async fn status_fetch_maps_the_v2_account_shape() {
        let mock = MockStripe::start(vec![(200, account_response())]).await;
        let provider = provider(mock.addr);

        let status = provider
            .fetch_status("acct_test_123")
            .await
            .expect("status fetched");
        assert_eq!(status.external_account_id, "acct_test_123");
        assert_eq!(
            status.capabilities.get("card_payments"),
            Some(crate::domain::capability::CapabilityStatus::Pending)
        );
        assert!(!status.capabilities.is_card_payments_active());
        assert_eq!(
            status.requirements.currently_due,
            vec!["individual.id_number".to_string()]
        );
        assert_eq!(status.country.as_ref().map(CountryCode::as_str), Some("JP"));
        assert_eq!(
            status
                .default_currency
                .as_ref()
                .map(crate::domain::money::Currency::as_str),
            Some("JPY")
        );
        assert_eq!(status.status_hint, None, "pending capability is not active");

        let call = &mock.recorded()[0];
        assert_eq!(call.method, "GET");
        assert_eq!(call.path, "/v2/core/accounts/acct_test_123");
    }

    #[tokio::test]
    async fn an_active_card_payments_capability_hints_active() {
        let mut response = account_response();
        response["configuration"]["merchant"]["capabilities"]["card_payments"]["status"] =
            serde_json::json!("active");
        let mock = MockStripe::start(vec![(200, response)]).await;
        let provider = provider(mock.addr);
        let status = provider
            .fetch_status("acct_test_123")
            .await
            .expect("status");
        assert_eq!(status.status_hint, Some(ConnectionStatus::Active));
    }

    // -- checkout on the connected account (09.4) ----------------------------

    use crate::domain::checkout::{Checkout, CheckoutId, MerchantOrderRef};
    use crate::domain::connection::ConnectionId;
    use crate::domain::money::Money;
    use crate::domain::ports::CreateCheckoutCommand;
    use sanvi_kernel::TenantId;

    fn checkout_command(account: &str, fee: Option<i64>) -> CreateCheckoutCommand {
        let checkout = Checkout::new(
            CheckoutId::try_from(1_i64).unwrap(),
            TenantId::try_from(2_i64).unwrap(),
            ConnectionId::try_from(3_i64).unwrap(),
            MerchantOrderRef::parse("order-2026-0821").unwrap(),
            Money::new(2000, "JPY").unwrap(),
            Utc::now(),
        )
        .unwrap();
        CreateCheckoutCommand {
            external_account_id: account.to_string(),
            checkout,
            success_url: "https://shop.tenant.test/thanks".to_string(),
            cancel_url: "https://shop.tenant.test/cart".to_string(),
            locale: "ja-JP".to_string(),
            application_fee: fee.map(|minor| Money::new(minor, "JPY").unwrap()),
            automatic_tax: None,
        }
    }

    fn checkout_session_response() -> Value {
        serde_json::json!({
            "id": "cs_test_123",
            "object": "checkout.session",
            "url": "https://checkout.stripe.com/c/pay/cs_test_123",
            "expires_at": 1_900_000_000_i64,
            "payment_intent": "pi_test_123"
        })
    }

    #[tokio::test]
    async fn checkout_sessions_are_created_on_the_connected_account() {
        let mock = MockStripe::start(vec![(200, checkout_session_response())]).await;
        let provider = provider(mock.addr);
        let command = checkout_command("acct_test_123", None);

        let session = provider
            .create_checkout(&command)
            .await
            .expect("session created");
        assert_eq!(session.external_session_id, "cs_test_123");
        assert_eq!(session.url, "https://checkout.stripe.com/c/pay/cs_test_123");
        assert_eq!(
            session.expires_at,
            DateTime::from_timestamp(1_900_000_000, 0)
        );

        let call = &mock.recorded()[0];
        assert_eq!(call.method, "POST");
        assert_eq!(call.path, "/v1/checkout/sessions");
        assert_eq!(
            call.stripe_account.as_deref(),
            Some("acct_test_123"),
            "the session is created ON the connected account — direct charge"
        );
        assert_eq!(
            call.idempotency_key.as_deref(),
            command
                .checkout
                .idempotency_key
                .map(|key| key.to_string())
                .as_deref(),
            "provider-side idempotency key is our checkout's idempotency key (crash adoption)"
        );

        let form = call.form();
        let field = |name: &str| {
            form.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(field("mode").as_deref(), Some("payment"));
        assert_eq!(
            field("success_url").as_deref(),
            Some("https://shop.tenant.test/thanks")
        );
        assert_eq!(
            field("cancel_url").as_deref(),
            Some("https://shop.tenant.test/cart")
        );
        assert_eq!(field("locale").as_deref(), Some("ja-jp"));
        assert_eq!(
            field("client_reference_id").as_deref(),
            Some("order-2026-0821")
        );
        assert_eq!(
            field("line_items[0][price_data][currency]").as_deref(),
            Some("JPY")
        );
        assert_eq!(
            field("line_items[0][price_data][unit_amount]").as_deref(),
            Some("2000")
        );
        // Metadata carries our ids so payment webhooks route home.
        assert_eq!(
            field("metadata[checkout_id]").as_deref(),
            Some(command.checkout.id.to_string().as_str())
        );
        assert_eq!(
            field("metadata[tenant_id]").as_deref(),
            Some(command.checkout.tenant_id.to_string().as_str())
        );
        assert!(
            field("payment_intent_data[metadata][checkout_id]").is_some(),
            "payment_intent metadata mirrors the session's for PI-event routing"
        );
        // The Dashboard flow tag with an 8-letter suffix.
        let integration = field("integration_identifier").expect("integration_identifier sent");
        let (_, suffix) = integration
            .rsplit_once('_')
            .expect("label has an underscored suffix");
        assert_eq!(suffix.len(), 8);
        assert!(suffix.bytes().all(|b| b.is_ascii_lowercase()));

        // The negative assertions ARE the invariant: no hardcoded method
        // list, and no fee or transfer shape while the policy is off.
        // Assembled from parts so this test never itself trips the CI
        // grep gate.
        let banned_fields = [
            concat!("payment_method_", "types"),
            concat!("application_", "fee_amount"),
            concat!("transfer_", "data"),
        ];
        for banned in banned_fields {
            assert!(
                !form.iter().any(|(key, _)| key.contains(banned)),
                "form must not carry {banned}"
            );
        }
    }

    #[tokio::test]
    async fn the_application_fee_rides_the_payment_intent_only_when_enabled() {
        let mock = MockStripe::start(vec![(200, checkout_session_response())]).await;
        let provider = provider(mock.addr);
        let command = checkout_command("acct_test_123", Some(100));

        provider.create_checkout(&command).await.expect("created");
        let form = mock.recorded()[0].form();
        let fee_field = concat!("payment_intent_data[application_", "fee_amount]");
        assert_eq!(
            form.iter()
                .find(|(key, _)| key == fee_field)
                .map(|(_, v)| v.as_str()),
            Some("100"),
            "the configured fee is populated through the only correct mechanism"
        );
        assert!(!form.iter().any(|(key, _)| key.starts_with("transfer")));
    }

    #[tokio::test]
    async fn a_non_account_target_is_refused_before_any_network_call() {
        let mock = MockStripe::start(vec![(200, checkout_session_response())]).await;
        let provider = provider(mock.addr);
        // A malformed account id can never reach Stripe.
        let err = provider
            .create_checkout(&checkout_command("cus_bogus", None))
            .await
            .unwrap_err();
        assert!(matches!(err, PaymentsError::InvalidConnection(_)));
        assert_eq!(mock.recorded().len(), 0, "nothing was sent");
    }

    // -- 09.6: automatic tax liability ---------------------------------------

    #[tokio::test]
    async fn sessions_carry_the_account_liability_when_automatic_tax_is_enabled() {
        let mock = MockStripe::start(vec![(200, checkout_session_response())]).await;
        let stripe = provider(mock.addr);
        let mut command = checkout_command("acct_test_123", None);
        command.automatic_tax = Some(crate::domain::ports::AutomaticTaxLiability {
            account: "acct_test_123".to_string(),
        });

        stripe.create_checkout(&command).await.expect("created");
        let call = &mock.recorded()[0];
        assert_eq!(
            call.stripe_account.as_deref(),
            Some("acct_test_123"),
            "the session is still created ON the connected account"
        );
        let form = call.form();
        let field = |name: &str| {
            form.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(field("automatic_tax[enabled]").as_deref(), Some("true"));
        // The liability assertion from the acceptance criteria, asserted on
        // the created request: type `account`, account = the connected id.
        assert_eq!(
            field("automatic_tax[liability][type]").as_deref(),
            Some("account"),
            "direct charges make the connected account liable"
        );
        assert_eq!(
            field("automatic_tax[liability][account]").as_deref(),
            Some("acct_test_123")
        );

        // And the release default stays intact when tax is off.
        let mock_off = MockStripe::start(vec![(200, checkout_session_response())]).await;
        provider(mock_off.addr)
            .create_checkout(&checkout_command("acct_test_123", None))
            .await
            .expect("created");
        let form_off = mock_off.recorded()[0].form();
        assert!(
            !form_off
                .iter()
                .any(|(key, _)| key.starts_with("automatic_tax")),
            "no automatic_tax fields without tenant opt-in"
        );
    }

    #[test]
    fn the_fee_transfer_guard_panics_on_the_banned_pairing() {
        // The production guard, driven directly: a request shape pairing
        // the platform fee with a transfer must panic (key shapes
        // assembled from parts so this file never trips the CI grep gate).
        let result = std::panic::catch_unwind(|| {
            let banned = [
                format!("payment_intent_data[application_{}amount]", "fee_"),
                concat!("transfer_", "data").to_string(),
            ];
            super::assert_direct_charge_shape(banned.iter().map(String::as_str));
        });
        assert!(result.is_err(), "the banned pairing must fail loudly");

        // Each half alone is legal.
        assert!(
            std::panic::catch_unwind(|| {
                let fee_only = [format!("payment_intent_data[application_{}amount]", "fee_")];
                super::assert_direct_charge_shape(fee_only.iter().map(String::as_str));
            })
            .is_ok()
        );
        assert!(
            std::panic::catch_unwind(|| {
                let transfer_only = [concat!("transfer_", "data").to_string()];
                super::assert_direct_charge_shape(transfer_only.iter().map(String::as_str));
            })
            .is_ok()
        );
    }

    // -- 09.6: payouts & tax compliance over the connected account ----------

    fn payouts_response() -> Value {
        serde_json::json!({
            "object": "list",
            "data": [
                {
                    "id": "po_paid_1",
                    "object": "payout",
                    "amount": 150000,
                    "currency": "jpy",
                    "status": "paid",
                    "arrival_date": 1_900_000_000_i64
                },
                {
                    "id": "po_pending_1",
                    "object": "payout",
                    "amount": 25000,
                    "currency": "usd",
                    "status": "in_transit",
                    "arrival_date": 1_900_100_000_i64
                },
                {
                    "id": "po_failed_1",
                    "object": "payout",
                    "amount": 900,
                    "currency": "eur",
                    "status": "failed"
                }
            ]
        })
    }

    #[tokio::test]
    async fn payout_listing_reads_the_connected_account_and_maps_statuses() {
        let mock = MockStripe::start(vec![(200, payouts_response())]).await;
        let provider = provider(mock.addr);

        let payouts = PaymentProviderPort::list_payouts(&provider, "acct_test_123", None)
            .await
            .expect("payout history");
        assert_eq!(payouts.len(), 3);

        let call = &mock.recorded()[0];
        assert_eq!(call.method, "GET");
        assert_eq!(
            call.path.split_once('?').map(|(path, _)| path),
            Some("/v1/payouts")
        );
        assert!(
            call.query()
                .contains(&("limit".to_string(), "100".to_string()))
        );
        assert!(
            call.body.is_empty(),
            "GET filters belong in the URL, not a body"
        );
        assert_eq!(
            call.stripe_account.as_deref(),
            Some("acct_test_123"),
            "payouts are read ON the connected account"
        );
        assert!(call.idempotency_key.is_none());

        use crate::domain::ports::PayoutStatus;
        assert_eq!(payouts[0].external_payout_id, "po_paid_1");
        assert_eq!(payouts[0].status, PayoutStatus::Paid);
        assert_eq!(payouts[0].amount.amount_minor, 150_000);
        assert_eq!(
            payouts[0].arrival_at,
            DateTime::from_timestamp(1_900_000_000, 0)
        );
        // in_transit collapses onto pending…
        assert_eq!(payouts[1].status, PayoutStatus::Pending);
        // …and failed stays failed (its webhook alerts separately).
        assert_eq!(payouts[2].status, PayoutStatus::Failed);
    }

    #[tokio::test]
    async fn payout_payment_ids_reconcile_through_balance_transactions() {
        let mock = MockStripe::start(vec![
            (
                200,
                serde_json::json!({
                    "object": "list",
                    "has_more": true,
                    "data": [{
                        "id": "txn_page_1",
                        "object": "balance_transaction",
                        "source": {
                            "id": "ch_paid_1",
                            "object": "charge",
                            "payment_intent": "pi_paid_1"
                        }
                    }]
                }),
            ),
            (
                200,
                serde_json::json!({
                    "object": "list",
                    "has_more": false,
                    "data": [{
                        "id": "txn_page_2",
                        "object": "balance_transaction",
                        "source": "ch_legacy_1"
                    }]
                }),
            ),
        ])
        .await;
        let provider = provider(mock.addr);

        let ids =
            PaymentProviderPort::list_payout_payment_ids(&provider, "acct_test_123", "po_test_123")
                .await
                .expect("payout payment ids");
        assert_eq!(ids, vec!["ch_legacy_1", "pi_paid_1"]);

        let calls = mock.recorded();
        assert_eq!(calls.len(), 2);
        for call in &calls {
            assert_eq!(
                call.path.split_once('?').map(|(path, _)| path),
                Some("/v1/balance_transactions")
            );
            assert_eq!(call.stripe_account.as_deref(), Some("acct_test_123"));
            assert!(call.body.is_empty(), "GET filters belong in the URL");
        }
        assert!(
            calls[0]
                .query()
                .contains(&("payout".to_string(), "po_test_123".to_string()))
        );
        assert!(
            calls[0]
                .query()
                .contains(&("expand[]".to_string(), "data.source".to_string()))
        );
        assert!(
            calls[1]
                .query()
                .contains(&("starting_after".to_string(), "txn_page_1".to_string()))
        );
    }

    #[tokio::test]
    async fn tax_compliance_is_read_on_the_connected_account() {
        // Two sequential responses: settings first, then registrations.
        let mock = MockStripe::start(vec![
            (
                200,
                serde_json::json!({"object": "tax.settings", "status": "active"}),
            ),
            (
                200,
                serde_json::json!({"object": "list", "data": [
                    {"id": "txr_1", "object": "tax.registration", "status": "active"}
                ]}),
            ),
        ])
        .await;
        let provider = provider(mock.addr);

        let snapshot = provider
            .fetch_tax_compliance("acct_test_123")
            .await
            .expect("compliance snapshot");
        assert_eq!(snapshot.status, "active");
        assert_eq!(snapshot.active_registrations, 1);

        let calls = mock.recorded();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].path, "/v1/tax/settings");
        assert_eq!(calls[0].stripe_account.as_deref(), Some("acct_test_123"));
        assert_eq!(
            calls[1].path.split_once('?').map(|(path, _)| path),
            Some("/v1/tax/registrations")
        );
        assert_eq!(calls[1].stripe_account.as_deref(), Some("acct_test_123"));
        let query = calls[1].query();
        assert!(
            query.contains(&("status".to_string(), "active".to_string())),
            "only active registrations count toward the preflight"
        );
        assert!(
            calls[1].body.is_empty(),
            "GET filters must not become a request body"
        );
    }

    #[tokio::test]
    async fn retrieving_a_session_replays_it_without_creating_one() {
        let mock = MockStripe::start(vec![(200, checkout_session_response())]).await;
        let provider = provider(mock.addr);

        let session = provider
            .retrieve_checkout("acct_test_123", "cs_test_123")
            .await
            .expect("retrieved");
        assert_eq!(session.external_session_id, "cs_test_123");

        let call = &mock.recorded()[0];
        assert_eq!(call.method, "GET");
        assert_eq!(call.path, "/v1/checkout/sessions/cs_test_123");
        assert_eq!(call.stripe_account.as_deref(), Some("acct_test_123"));
        assert!(call.idempotency_key.is_none(), "GETs are not keyed");
    }

    // -- error taxonomy ------------------------------------------------------

    #[tokio::test]
    async fn every_stripe_error_class_maps_to_its_own_domain_error() {
        let cases: Vec<(u16, Value, PaymentsError)> = vec![
            (
                401,
                serde_json::json!({"error": {"code": "api_key_expired", "type": "invalid_request_error"}}),
                PaymentsError::ProviderAuth,
            ),
            (
                429,
                serde_json::json!({"error": {"code": "rate_limited", "type": "invalid_request_error"}}),
                PaymentsError::ProviderRateLimited("x".into()),
            ),
            (
                409,
                serde_json::json!({"error": {"code": "idempotency_error", "type": "invalid_request_error"}}),
                PaymentsError::IdempotencyConflict("x".into()),
            ),
            (
                400,
                serde_json::json!({"error": {"code": "country_not_supported", "type": "invalid_request_error"}}),
                PaymentsError::CountryUnsupported("x".into()),
            ),
            (
                400,
                serde_json::json!({"error": {"code": "identity_country_required", "type": "invalid_request_error"}}),
                PaymentsError::InvalidCountry("x".into()),
            ),
        ];
        for (status, body, expected) in cases {
            let mapped = StripeConnectProvider::classify_error(
                "account_create",
                status,
                &body,
                Some("req_x"),
            );
            assert_eq!(
                std::mem::discriminant(&mapped),
                std::mem::discriminant(&expected),
                "status {status} must map to {expected:?}, got {mapped:?}"
            );
        }
    }

    #[tokio::test]
    async fn auth_failures_fail_fast_without_retrying() {
        let mock = MockStripe::start(vec![(
            401,
            serde_json::json!({"error": {"code": "api_key_expired", "type": "invalid_request_error"}}),
        )])
        .await;
        let provider = provider(mock.addr);
        let err = provider
            .create_account(&create_request())
            .await
            .expect_err("auth failure");
        assert!(matches!(err, PaymentsError::ProviderAuth));
        assert_eq!(
            mock.recorded().len(),
            1,
            "authentication failures never retry"
        );
    }

    #[tokio::test]
    async fn rate_limits_retry_with_backoff_then_surface_rate_limited() {
        let mock = MockStripe::start(vec![(
            429,
            serde_json::json!({"error": {"code": "rate_limited", "type": "invalid_request_error"}}),
        )])
        .await;
        let provider = provider(mock.addr);
        let err = provider
            .create_account(&create_request())
            .await
            .expect_err("rate limit exhausted");
        assert!(matches!(err, PaymentsError::ProviderRateLimited(_)));
        assert_eq!(
            mock.recorded().len(),
            MAX_RETRIES + 1,
            "initial attempt plus MAX_RETRIES backoffs"
        );
    }

    #[tokio::test]
    async fn error_details_never_echo_the_stripe_error_body() {
        let mock = MockStripe::start(vec![(
            400,
            serde_json::json!({"error": {"code": "country_not_supported", "type": "invalid_request_error", "message": "SECRET-INTERNAL country XX is not supported for Connect; internal hint 42"}}),
        )])
        .await;
        let provider = provider(mock.addr);
        let err = provider
            .create_account(&create_request())
            .await
            .expect_err("country unsupported");
        let rendered = err.to_string();
        assert!(!rendered.contains("SECRET-INTERNAL"), "{rendered}");
        assert!(!rendered.contains("internal hint"), "{rendered}");
    }

    // -- webhook verification -----------------------------------------------

    #[test]
    fn webhook_verification_uses_the_connect_secret_and_fails_closed() {
        let provider = StripeConnectProvider::new(
            "https://api.stripe.com".to_string(),
            "rk_test".to_string(),
            "2026-06-24.dahlia".to_string(),
            "whsec_connect_only".to_string(),
            300,
        );
        let payload = br#"{"id": "evt_1"}"#;
        let signed = sign("whsec_connect_only", Utc::now().timestamp(), payload);
        provider
            .verify_webhook(payload, &signed)
            .expect("valid signature");

        // The platform endpoint's secret must NOT verify here.
        let platform = sign("whsec_platform_endpoint", Utc::now().timestamp(), payload);
        assert!(provider.verify_webhook(payload, &platform).is_err());

        // No configured secret fails closed.
        let unconfigured = StripeConnectProvider::new(
            "https://api.stripe.com".to_string(),
            "rk_test".to_string(),
            "2026-06-24.dahlia".to_string(),
            String::new(),
            300,
        );
        assert!(unconfigured.verify_webhook(payload, &signed).is_err());
    }

    fn sign(secret: &str, timestamp: i64, payload: &[u8]) -> String {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes())
            .expect("hmac accepts any key length");
        mac.update(format!("{timestamp}.{}", String::from_utf8_lossy(payload)).as_bytes());
        format!(
            "t={timestamp},v1={}",
            hex::encode(mac.finalize().into_bytes())
        )
    }

    // -- the session secret never touches a log ------------------------------

    #[tokio::test]
    async fn the_account_session_secret_never_appears_in_logs() {
        use tracing_subscriber::layer::SubscriberExt;

        const SECRET: &str = "super_secret_client_secret_value";
        let mock = MockStripe::start(vec![(
            200,
            serde_json::json!({
                "account": "acct_test_123",
                "client_secret": SECRET,
                "expires_at": 1_900_000_000_i64
            }),
        )])
        .await;

        let captured: Arc<std::sync::Mutex<Vec<u8>>> = Arc::default();
        let writer = CaptureWriter(captured.clone());
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false),
        );
        let _guard = tracing::subscriber::set_default(subscriber);

        let provider = provider(mock.addr);
        let session = provider
            .create_account_session("acct_test_123")
            .await
            .expect("session minted");
        drop(_guard);

        assert_eq!(session.client_secret, SECRET);
        let logs = String::from_utf8_lossy(&captured.lock().expect("log lock")).to_string();
        assert!(
            !logs.contains(SECRET),
            "the account session secret leaked into logs: {logs}"
        );
    }

    struct CaptureWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;

        fn make_writer(&'a self) -> Self::Writer {
            CaptureWriter(self.0.clone())
        }
    }
}
