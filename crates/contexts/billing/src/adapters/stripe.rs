//! The HTTP Stripe client: typed wrapper over `reqwest` with a pinned API
//! version, idempotency keys on every write, timeouts, retries and an error
//! taxonomy the application can act on. Also: webhook signature
//! verification (HMAC-SHA256 over `t.payload`, tolerance-checked) and the
//! cached tax gate.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sanvi_kernel::TenantId;
use sha2::Sha256;
use uuid::Uuid;

use crate::domain::{
    BillingError, CheckoutSessionRequest, CheckoutSessionResult, PortalSessionRequest,
    PortalSessionResult, StripeClient, StripeCustomer, StripeSubscriptionView, TaxGate,
    TaxRegistration,
};

type HmacSha256 = Hmac<Sha256>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RETRIES: usize = 3;
const RETRY_BACKOFF_MS: u64 = 200;

/// One attempt's failure, classified by the retry rule: retryable
/// attempts feed the bounded in-call backoff, fatal ones surface
/// immediately.
enum AttemptOutcome {
    Retryable(BillingError),
    Fatal(BillingError),
}

/// Verify a Stripe webhook signature header (`t=...,v1=...`) against the
/// signing secret. Pure function — unit-tested against known vectors.
pub fn verify_webhook_signature(
    secret: &[u8],
    payload: &[u8],
    signature_header: &str,
    tolerance_secs: i64,
) -> Result<(), BillingError> {
    let mut timestamp: Option<i64> = None;
    let mut signature: Option<&str> = None;
    for part in signature_header.split(',') {
        let Some((key, value)) = part.trim().split_once('=') else {
            return Err(BillingError::InvalidWebhookSignature(
                "malformed signature header".to_string(),
            ));
        };
        match key {
            "t" => {
                timestamp = value.parse().ok();
            }
            "v1" => signature = Some(value),
            _ => {}
        }
    }
    let (Some(timestamp), Some(signature)) = (timestamp, signature) else {
        return Err(BillingError::InvalidWebhookSignature(
            "signature header missing t or v1".to_string(),
        ));
    };

    let now = chrono::Utc::now().timestamp();
    if (now - timestamp).abs() > tolerance_secs {
        return Err(BillingError::InvalidWebhookSignature(format!(
            "timestamp {timestamp} outside tolerance ({tolerance_secs}s)"
        )));
    }

    let signed_payload = format!("{timestamp}.{}", String::from_utf8_lossy(payload));
    let mut mac = HmacSha256::new_from_slice(secret)
        .map_err(|_| BillingError::InvalidWebhookSignature("bad secret".to_string()))?;
    mac.update(signed_payload.as_bytes());
    let expected = mac.finalize().into_bytes();

    // Constant-time comparison against a decoded candidate.
    let Some(candidate) = decode_hex(signature) else {
        return Err(BillingError::InvalidWebhookSignature(
            "signature is not hex".to_string(),
        ));
    };
    if candidate.len() != expected.len() {
        return Err(BillingError::InvalidWebhookSignature(
            "signature length mismatch".to_string(),
        ));
    }
    let mut diff = 0u8;
    for (a, b) in candidate.iter().zip(expected.iter()) {
        diff |= a ^ b;
    }
    if diff != 0 {
        return Err(BillingError::InvalidWebhookSignature(
            "signature mismatch".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode_hex(raw: &str) -> Option<Vec<u8>> {
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    (0..raw.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&raw[i..i + 2], 16).ok())
        .collect()
}

/// Optional metrics: request duration per operation and error counts per
/// code. Both apps create the client; either may pass a registry.
#[derive(Clone)]
pub struct StripeMetrics {
    duration: prometheus::HistogramVec,
    errors: prometheus::IntCounterVec,
}

impl StripeMetrics {
    pub fn new(registry: &prometheus::Registry) -> Self {
        let duration = prometheus::HistogramVec::new(
            prometheus::HistogramOpts::new(
                "stripe_request_duration",
                "Stripe API call duration by operation",
            ),
            &["op"],
        )
        .expect("stripe duration metric construction is infallible");
        let errors = prometheus::IntCounterVec::new(
            prometheus::Opts::new("stripe_error_total", "Stripe API errors by code"),
            &["code"],
        )
        .expect("stripe error metric construction is infallible");
        let _ = registry.register(Box::new(duration.clone()));
        let _ = registry.register(Box::new(errors.clone()));
        Self { duration, errors }
    }

    fn observe(&self, op: &str, started: Instant) {
        self.duration
            .with_label_values(&[op])
            .observe(started.elapsed().as_secs_f64());
    }

    fn error(&self, code: &str) {
        self.errors.with_label_values(&[code]).inc();
    }
}

/// The real Stripe gateway.
pub struct HttpStripeClient {
    http: reqwest::Client,
    api_base: String,
    api_key: String,
    api_version: String,
    metrics: Option<StripeMetrics>,
    retry_backoff_ms: u64,
    breaker: Option<Arc<sanvi_resilience::CircuitBreaker>>,
    degradation: Option<Arc<sanvi_resilience::DegradationRegistry>>,
}

impl HttpStripeClient {
    pub fn new(
        api_base: String,
        api_key: String,
        api_version: String,
        metrics: Option<StripeMetrics>,
    ) -> Self {
        Self::new_with_resilience(
            api_base,
            api_key,
            api_version,
            metrics,
            CONNECT_TIMEOUT,
            REQUEST_TIMEOUT,
            RETRY_BACKOFF_MS,
            sanvi_resilience::BreakerConfig::default(),
            None,
        )
    }

    /// The composition-root constructor (TASK-023): production timeouts
    /// and the matrix breaker (5 failures / 30 s cooldown), plus the
    /// shared degradation registry so the `stripe` reason reaches the
    /// `X-Sanvi-Degraded` header while the breaker is open.
    pub fn new_with_degradation(
        api_base: String,
        api_key: String,
        api_version: String,
        metrics: Option<StripeMetrics>,
        degradation: std::sync::Arc<sanvi_resilience::DegradationRegistry>,
    ) -> Self {
        Self::new_with_resilience(
            api_base,
            api_key,
            api_version,
            metrics,
            CONNECT_TIMEOUT,
            REQUEST_TIMEOUT,
            RETRY_BACKOFF_MS,
            sanvi_resilience::BreakerConfig::default(),
            Some(degradation),
        )
    }

    /// Test/full-control constructor: the production one pins the matrix
    /// constants; injection tests shorten the deadlines and count faster
    /// so a slow Stripe is observable in milliseconds.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_timeouts(
        api_base: String,
        api_key: String,
        api_version: String,
        metrics: Option<StripeMetrics>,
        connect_timeout: Duration,
        request_timeout: Duration,
        retry_backoff_ms: u64,
    ) -> Self {
        Self::new_with_resilience(
            api_base,
            api_key,
            api_version,
            metrics,
            connect_timeout,
            request_timeout,
            retry_backoff_ms,
            sanvi_resilience::BreakerConfig::default(),
            None,
        )
    }

    /// The full constructor (TASK-023): a circuit breaker per the
    /// dependency matrix (failures/cooldown in `config`) and the
    /// degradation registry for the `stripe` reason.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_resilience(
        api_base: String,
        api_key: String,
        api_version: String,
        metrics: Option<StripeMetrics>,
        connect_timeout: Duration,
        request_timeout: Duration,
        retry_backoff_ms: u64,
        breaker_config: sanvi_resilience::BreakerConfig,
        degradation: Option<Arc<sanvi_resilience::DegradationRegistry>>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .build()
            .expect("stripe http client construction is infallible");
        let breaker = Arc::new(sanvi_resilience::CircuitBreaker::new(
            "stripe",
            breaker_config,
            None,
        ));
        Self {
            http,
            api_base,
            api_key,
            api_version,
            metrics,
            retry_backoff_ms,
            breaker: Some(breaker),
            degradation,
        }
    }

    /// Mark the registry's `stripe` reason while the breaker is open.
    fn note_breaker_state(&self) {
        if let (Some(breaker), Some(degradation)) = (&self.breaker, &self.degradation) {
            degradation.set_degraded(
                sanvi_resilience::DegradationReason::Stripe,
                breaker.state() == sanvi_resilience::BreakerState::Open,
            );
        }
    }

    async fn post_form(
        &self,
        op: &str,
        path: &str,
        idempotency_key: Option<&str>,
        form: &[(&str, String)],
    ) -> Result<serde_json::Value, BillingError> {
        let started = Instant::now();
        let url = format!("{}{}", self.api_base.trim_end_matches('/'), path);

        // TASK-023: the call runs under the dependency-matrix breaker, one
        // report per call (retries are in-call backoff, not separate
        // outcomes). An open breaker fails fast with the same error shape
        // a failing call produces — callers cannot tell a tripped breaker
        // from a refusing Stripe, and neither should have to.
        let mut permit = match self.breaker.as_ref().map(|breaker| breaker.acquire()) {
            Some(Ok(permit)) => Some(permit),
            Some(Err(_open)) => {
                self.note_breaker_state();
                return Err(BillingError::StripeConnection {
                    detail: "circuit breaker open after repeated Stripe failures".to_string(),
                });
            }
            None => None,
        };

        let mut last_err = None;
        for attempt in 0..=MAX_RETRIES {
            match self
                .post_form_attempt(op, &url, idempotency_key, form, started)
                .await
            {
                Ok(body) => {
                    if let Some(permit) = permit.take() {
                        permit.report(true);
                    }
                    return Ok(body);
                }
                Err(AttemptOutcome::Fatal(err)) => {
                    if let Some(permit) = permit.take() {
                        permit.report(false);
                    }
                    return Err(err);
                }
                Err(AttemptOutcome::Retryable(err)) => {
                    last_err = Some(err);
                    if attempt < MAX_RETRIES {
                        tokio::time::sleep(Duration::from_millis(
                            self.retry_backoff_ms * 2u64.pow(attempt as u32),
                        ))
                        .await;
                    }
                }
            }
        }
        if let Some(permit) = permit.take() {
            permit.report(false);
        }
        self.note_breaker_state();
        Err(last_err.unwrap_or_else(|| BillingError::StripeConnection {
            detail: "unknown failure".to_string(),
        }))
    }

    /// One HTTP attempt, classified into retryable (429/409/5xx/transport)
    /// and fatal (everything else) by the same rule the matrix documents.
    async fn post_form_attempt(
        &self,
        op: &str,
        url: &str,
        idempotency_key: Option<&str>,
        form: &[(&str, String)],
        started: Instant,
    ) -> Result<serde_json::Value, AttemptOutcome> {
        let mut request = self
            .http
            .post(url)
            .bearer_auth(&self.api_key)
            .header("Stripe-Version", &self.api_version)
            .form(form);
        if let Some(key) = idempotency_key {
            request = request.header("Idempotency-Key", key);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(err) => {
                return Err(AttemptOutcome::Retryable(BillingError::StripeConnection {
                    detail: err.to_string(),
                }));
            }
        };
        let status = response.status();
        let body: serde_json::Value = response
            .json()
            .await
            .unwrap_or_else(|_| serde_json::json!({}));
        if let Some(metrics) = &self.metrics {
            metrics.observe(op, started);
        }
        if status.is_success() {
            return Ok(body);
        }
        let code = status.as_u16();
        if let Some(metrics) = &self.metrics {
            metrics.error(&code.to_string());
        }
        let err = match code {
            401 | 403 => BillingError::StripeUnauthorized,
            429 => BillingError::StripeRateLimited,
            400 | 404 => BillingError::StripeBadRequest {
                detail: stripe_message(&body).to_string(),
            },
            _ => BillingError::StripeService {
                detail: stripe_message(&body).to_string(),
            },
        };
        // Retry on rate limits, conflicts and service failures; never on
        // authentication or validation problems.
        if code == 429 || code == 409 || code >= 500 {
            return Err(AttemptOutcome::Retryable(err));
        }
        Err(AttemptOutcome::Fatal(err))
    }

    async fn get_json(&self, op: &str, path: &str) -> Result<serde_json::Value, BillingError> {
        let started = Instant::now();
        let url = format!("{}{}", self.api_base.trim_end_matches('/'), path);
        let response = self
            .http
            .get(&url)
            .bearer_auth(&self.api_key)
            .header("Stripe-Version", &self.api_version)
            .send()
            .await
            .map_err(|err| BillingError::StripeConnection {
                detail: err.to_string(),
            })?;
        let status = response.status();
        let body: serde_json::Value = response
            .json()
            .await
            .unwrap_or_else(|_| serde_json::json!({}));
        if let Some(metrics) = &self.metrics {
            metrics.observe(op, started);
        }
        if status.is_success() {
            return Ok(body);
        }
        let code = status.as_u16();
        if let Some(metrics) = &self.metrics {
            metrics.error(&code.to_string());
        }
        match code {
            401 | 403 => Err(BillingError::StripeUnauthorized),
            429 => Err(BillingError::StripeRateLimited),
            400 | 404 => Err(BillingError::StripeBadRequest {
                detail: stripe_message(&body).to_string(),
            }),
            _ => Err(BillingError::StripeService {
                detail: stripe_message(&body).to_string(),
            }),
        }
    }
}

fn stripe_message(body: &serde_json::Value) -> &str {
    body.get("error")
        .and_then(|error| error.get("message"))
        .and_then(|message| message.as_str())
        .unwrap_or("no error detail")
}

fn parse_epoch(value: Option<i64>) -> Option<chrono::DateTime<chrono::Utc>> {
    value.and_then(|epoch| chrono::DateTime::from_timestamp(epoch, 0))
}

#[async_trait]
impl StripeClient for HttpStripeClient {
    async fn ensure_customer(
        &self,
        tenant_id: TenantId,
        command_id: Uuid,
    ) -> Result<StripeCustomer, BillingError> {
        let body = self
            .post_form(
                "customers.create",
                "/v1/customers",
                Some(&format!("sanvi_customer_{command_id}")),
                &[
                    ("metadata[sanvi_tenant_id]", tenant_id.to_string()),
                    ("description", format!("Sanvi tenant {tenant_id}")),
                ],
            )
            .await?;
        let id = body
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| BillingError::StripeService {
                detail: "customer response missing id".to_string(),
            })?
            .to_string();
        Ok(StripeCustomer { id })
    }

    async fn create_checkout_session(
        &self,
        request: &CheckoutSessionRequest,
    ) -> Result<CheckoutSessionResult, BillingError> {
        let mut form: Vec<(&'static str, String)> = vec![
            ("mode", "subscription".to_string()),
            ("customer", request.customer_id.clone()),
            ("line_items[0][price]", request.stripe_price_id.clone()),
            ("line_items[0][quantity]", "1".to_string()),
            (
                "subscription_data[metadata][sanvi_tenant_id]",
                request.tenant_id.to_string(),
            ),
            ("success_url", request.success_url.clone()),
            ("cancel_url", request.cancel_url.clone()),
            ("client_reference_id", request.tenant_id.to_string()),
            (
                "integration_identifier",
                format!("sanvi_checkout_{}", request.integration_identifier_suffix),
            ),
            // Never payment_method_types: dynamic payment methods maximise
            // conversion and are configured in the Dashboard.
            // B2B reverse-charge: collect tax ids.
            ("tax_id_collection[enabled]", "true".to_string()),
        ];
        if let Some(trial_days) = request.trial_period_days {
            form.push((
                "subscription_data[trial_period_days]",
                trial_days.to_string(),
            ));
        }
        if request.automatic_tax {
            form.push(("automatic_tax[enabled]", "true".to_string()));
        }
        let body = self
            .post_form(
                "checkout.sessions.create",
                "/v1/checkout/sessions",
                Some(&format!("sanvi_checkout_{}", request.command_id)),
                &form,
            )
            .await?;
        Ok(CheckoutSessionResult {
            session_id: body
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            url: body
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| BillingError::StripeService {
                    detail: "checkout session response missing url".to_string(),
                })?
                .to_string(),
        })
    }

    async fn create_portal_session(
        &self,
        request: &PortalSessionRequest,
    ) -> Result<PortalSessionResult, BillingError> {
        let body = self
            .post_form(
                "billing_portal.sessions.create",
                "/v1/billing_portal/sessions",
                Some(&format!("sanvi_portal_{}", request.command_id)),
                &[
                    ("customer", request.customer_id.clone()),
                    ("return_url", request.return_url.clone()),
                ],
            )
            .await?;
        Ok(PortalSessionResult {
            url: body
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| BillingError::StripeService {
                    detail: "portal session response missing url".to_string(),
                })?
                .to_string(),
        })
    }

    async fn retrieve_subscription(
        &self,
        stripe_subscription_id: &str,
    ) -> Result<StripeSubscriptionView, BillingError> {
        let body = self
            .get_json(
                "subscriptions.retrieve",
                &format!("/v1/subscriptions/{stripe_subscription_id}"),
            )
            .await?;
        let status = body.get("status").and_then(|v| v.as_str()).ok_or_else(|| {
            BillingError::StripeService {
                detail: "subscription response missing status".to_string(),
            }
        })?;
        let stripe_price_id = body
            .get("items")
            .and_then(|items| items.get("data"))
            .and_then(|data| data.as_array())
            .and_then(|items| items.first())
            .and_then(|item| item.get("price"))
            .and_then(|price| price.get("id"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok(StripeSubscriptionView {
            id: body
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            customer_id: body
                .get("customer")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            status: crate::domain::SubscriptionStatus::parse(status)?,
            stripe_price_id,
            current_period: crate::domain::period_from_range(
                body.get("current_period_start").and_then(|v| v.as_i64()),
                body.get("current_period_end").and_then(|v| v.as_i64()),
            ),
            cancel_at_period_end: body
                .get("cancel_at_period_end")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            trial_end: parse_epoch(body.get("trial_end").and_then(|v| v.as_i64())),
            updated: parse_epoch(body.get("updated").and_then(|v| v.as_i64())),
        })
    }

    async fn delete_customer(&self, stripe_customer_id: &str) -> Result<(), BillingError> {
        let response = self
            .http
            .delete(format!(
                "{}/v1/customers/{}",
                self.api_base.trim_end_matches('/'),
                stripe_customer_id
            ))
            .bearer_auth(&self.api_key)
            .header("Stripe-Version", &self.api_version)
            .send()
            .await
            .map_err(|err| BillingError::StripeConnection {
                detail: err.to_string(),
            })?;
        let status = response.status();
        let body: serde_json::Value = response
            .json()
            .await
            .unwrap_or_else(|_| serde_json::json!({}));
        // A deleted (or already-gone) customer is the desired end state.
        if status.is_success() || (body.get("deleted").and_then(|d| d.as_bool()) == Some(true)) {
            return Ok(());
        }
        let code = status.as_u16();
        match code {
            404 => Ok(()),
            401 | 403 => Err(BillingError::StripeUnauthorized),
            429 => Err(BillingError::StripeRateLimited),
            400 => Err(BillingError::StripeBadRequest {
                detail: stripe_message(&body).to_string(),
            }),
            _ => Err(BillingError::StripeService {
                detail: stripe_message(&body).to_string(),
            }),
        }
    }

    async fn list_active_tax_registrations(&self) -> Result<Vec<TaxRegistration>, BillingError> {
        let body = self
            .get_json(
                "tax.registrations.list",
                "/v1/tax/registrations?status=active&limit=100",
            )
            .await?;
        let mut registrations = Vec::new();
        if let Some(data) = body.get("data").and_then(|d| d.as_array()) {
            for registration in data {
                let id = registration
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let country = registration
                    .get("country")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                registrations.push(TaxRegistration {
                    id: id.to_string(),
                    country: country.to_string(),
                });
            }
        }
        Ok(registrations)
    }
}

/// The tax gate with a short-lived cache: the registration list changes
/// rarely, but every Checkout creation must answer it.
pub struct CachedTaxGate {
    stripe: Arc<dyn StripeClient>,
    ttl: Duration,
    cache: Mutex<Option<(Instant, bool)>>,
}

impl CachedTaxGate {
    pub fn new(stripe: Arc<dyn StripeClient>, ttl: Duration) -> Self {
        Self {
            stripe,
            ttl,
            cache: Mutex::new(None),
        }
    }
}

#[async_trait]
impl TaxGate for CachedTaxGate {
    async fn has_active_registration(&self) -> Result<bool, BillingError> {
        {
            let cache = self.cache.lock().expect("tax gate cache is not poisoned");
            if let Some((at, answer)) = *cache
                && at.elapsed() < self.ttl
            {
                return Ok(answer);
            }
        }
        let answer = !self
            .stripe
            .list_active_tax_registrations()
            .await?
            .is_empty();
        *self.cache.lock().expect("tax gate cache is not poisoned") =
            Some((Instant::now(), answer));
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &str, timestamp: i64, payload: &[u8]) -> String {
        let signed_payload = format!("{timestamp}.{}", String::from_utf8_lossy(payload));
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(signed_payload.as_bytes());
        format!("t={timestamp},v1={}", hex(&mac.finalize().into_bytes()))
    }

    #[test]
    fn valid_signature_passes() {
        let secret = "whsec_test";
        let payload = br#"{"id": "evt_1"}"#;
        let header = sign(secret, chrono::Utc::now().timestamp(), payload);
        verify_webhook_signature(secret.as_bytes(), payload, &header, 300).unwrap();
    }

    #[test]
    fn wrong_secret_or_tampered_payload_fails() {
        let secret = "whsec_test";
        let payload = br#"{"id": "evt_1"}"#;
        let header = sign(secret, chrono::Utc::now().timestamp(), payload);
        verify_webhook_signature(b"whsec_other", payload, &header, 300).unwrap_err();
        let tampered = br#"{"id": "evt_2"}"#;
        verify_webhook_signature(secret.as_bytes(), tampered, &header, 300).unwrap_err();
    }

    #[test]
    fn stale_timestamps_fail() {
        let secret = "whsec_test";
        let payload = br#"{"id": "evt_1"}"#;
        let stale = chrono::Utc::now().timestamp() - 10_000;
        let header = sign(secret, stale, payload);
        verify_webhook_signature(secret.as_bytes(), payload, &header, 300).unwrap_err();
    }

    #[test]
    fn malformed_headers_fail() {
        let secret = "whsec_test";
        let payload = br#"{"id": "evt_1"}"#;
        verify_webhook_signature(secret.as_bytes(), payload, "v1=abc", 300).unwrap_err();
        verify_webhook_signature(secret.as_bytes(), payload, "t=123,v1=zz", 300).unwrap_err();
        verify_webhook_signature(secret.as_bytes(), payload, "", 300).unwrap_err();
    }
}
