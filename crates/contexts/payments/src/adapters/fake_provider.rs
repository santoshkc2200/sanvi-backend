//! The fake payment provider: the proof the provider abstraction holds.
//!
//! In-memory, deterministic, scriptable. It exists so (a) tests drive
//! connect → active → checkout → refund without network or credentials,
//! and (b) the frontend can build 09.2–09.6 against a working catalog and
//! onboarding flow before Stripe sandbox credentials exist in every
//! developer's environment. Treat it as a deliverable to the other repo,
//! not as test scaffolding.
//!
//! ## Why it cannot ship enabled
//!
//! The adapter is *registered* only in non-production environments: the
//! composition root consults the deployment environment before putting it
//! in the registry, and [`super::super::application::catalog`]
//! independently refuses to list `production: false` descriptors when the
//! environment is `production` — two gates, so a mistake in one is not
//! enough. A production-configured process therefore has no `fake` entry
//! in its catalog and no code path that could create one; the same binary
//! pointed at a development environment gets the fake for free.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};

use crate::domain::capability::{CapabilitySet, CapabilityStatus, RequirementSummary};
use crate::domain::error::PaymentsError;
use crate::domain::money::Money;
use crate::domain::payment::RefundStatus;
use crate::domain::ports::{
    CreateAccountRequest, CreateCheckoutCommand, PaymentProviderPort, PayoutReadPort, PayoutStatus,
    ProviderAccount, ProviderAccountSession, ProviderAccountStatus, ProviderCheckoutSession,
    ProviderDescriptor, ProviderPayout, ProviderRefund, ProviderRefundCommand, WebhookVerifierPort,
};
use crate::domain::provider::{ConnectionStatus, CountryCode, ProviderKind};

/// The deterministic signing secret the fake verifies against. Tests and
/// dev traffic sign with this; anything else fails — including a real
/// Stripe secret, which must never verify against the fake (and vice
/// versa).
pub const FAKE_WEBHOOK_SECRET: &str = "whsec_fake_provider";

/// What the fake has been told to do next. Everything is optional; the
/// unscripted default is the happy path (accounts activate on sync once
/// `approve_onboarding` is called, refunds succeed).
#[derive(Debug, Clone, Default)]
pub struct FakeScript {
    /// Reports this status from the next `fetch_status`/`sync`, whatever
    /// the internal state says — how tests produce an out-of-order or
    /// surprising status.
    pub next_status_override: Option<ConnectionStatus>,
    /// Requirements the account will report as currently due.
    pub required_fields: Vec<String>,
    /// Set the `card_payments` capability to this on the next sync.
    pub card_payments: Option<CapabilityStatus>,
    /// Fail the next refund with this message.
    pub fail_next_refund: Option<String>,
    /// The next minted account session is already expired — the
    /// use case must refuse it instead of handing on a dead credential.
    pub next_session_expired: bool,
}

struct FakeAccount {
    external_account_id: String,
    capabilities: CapabilitySet,
    requirements: RequirementSummary,
    onboarding_submitted: bool,
}

/// The fake provider adapter. `Clone` shares state (it is one provider,
/// however many handles), so tests keep a handle to script it while the
/// registry holds another.
#[derive(Clone, Default)]
pub struct FakeProvider {
    state: std::sync::Arc<Mutex<FakeState>>,
    ids: std::sync::Arc<AtomicU64>,
}

#[derive(Default)]
struct FakeState {
    script: FakeScript,
    accounts: BTreeMap<String, FakeAccount>,
    /// Accounts created per caller idempotency key: the same key returns
    /// the same account, exactly like the provider-side idempotency the
    /// real adapter relies on for crash adoption.
    by_idempotency_key: BTreeMap<uuid::Uuid, String>,
    /// Live checkout sessions by their id — retrieval is the replay path
    /// (09.4): a retried request re-reads the same session instead of
    /// minting a second one.
    sessions: BTreeMap<String, ProviderCheckoutSession>,
    /// Refunds by caller idempotency key — the same key returns the same
    /// refund, mirroring the provider-side idempotency the refund use
    /// case's re-drive path relies on (a retry after a lost response
    /// must never move money twice).
    refunds_by_key: BTreeMap<uuid::Uuid, ProviderRefund>,
    /// How many refunds the provider actually executed (tests assert
    /// "a retried key issues exactly one refund" against this).
    refund_executions: u64,
}

impl FakeProvider {
    pub fn new() -> Self {
        Self::default()
    }

    fn descriptor() -> ProviderDescriptor {
        ProviderDescriptor {
            kind: ProviderKind::Fake,
            display_name: "Test provider".to_string(),
            supported_countries: ["US", "JP"]
                .iter()
                .map(|c| CountryCode::parse(c).expect("static country codes are valid"))
                .collect(),
            requires_onboarding: true,
            production: false,
        }
    }

    /// Tell the fake what to do next.
    pub fn script(&self, script: FakeScript) {
        let mut state = self.state.lock().expect("fake provider state lock");
        state.script = script;
    }

    /// The happy-path nudge: the account holder finished onboarding at
    /// the provider's embedded surface. The next sync reports
    /// `card_payments: active` and no requirements.
    pub fn approve_onboarding(&self, external_account_id: &str) {
        let mut state = self.state.lock().expect("fake provider state lock");
        if let Some(account) = state.accounts.get_mut(external_account_id) {
            account.onboarding_submitted = true;
            account.capabilities = CapabilitySet::empty()
                .with("card_payments", CapabilityStatus::Active)
                .with("transfers", CapabilityStatus::Active);
            account.requirements = RequirementSummary::default();
        }
    }

    /// How many refunds the provider actually executed (idempotent
    /// replays excluded) — the "exactly one refund at the provider"
    /// assertion for retried refund requests.
    pub fn refund_executions(&self) -> u64 {
        self.state
            .lock()
            .expect("fake provider state lock")
            .refund_executions
    }

    fn snapshot(
        &self,
        external_account_id: &str,
        purpose: &str,
    ) -> Result<ProviderAccountStatus, PaymentsError> {
        let state = self.state.lock().expect("fake provider state lock");
        let account = state.accounts.get(external_account_id).ok_or_else(|| {
            PaymentsError::ProviderUnavailable(format!(
                "fake provider knows no account {external_account_id:?} ({purpose})"
            ))
        })?;

        let (capabilities, requirements, status_hint) =
            if let Some(status) = state.script.next_status_override {
                // The scripted surprise: report exactly this, regardless of
                // internal state — the out-of-order/stale-status test case.
                let capabilities = match state.script.card_payments {
                    Some(CapabilityStatus::Active) => {
                        CapabilitySet::empty().with("card_payments", CapabilityStatus::Active)
                    }
                    _ => account.capabilities.clone(),
                };
                let requirements = if state.script.required_fields.is_empty() {
                    account.requirements.clone()
                } else {
                    RequirementSummary {
                        currently_due: state.script.required_fields.clone(),
                        ..RequirementSummary::default()
                    }
                };
                (capabilities, requirements, Some(status))
            } else {
                let status_hint = if account.onboarding_submitted {
                    Some(ConnectionStatus::Active)
                } else {
                    Some(ConnectionStatus::Onboarding)
                };
                (
                    account.capabilities.clone(),
                    account.requirements.clone(),
                    status_hint,
                )
            };

        Ok(ProviderAccountStatus {
            external_account_id: account.external_account_id.clone(),
            capabilities,
            requirements,
            country: Some(CountryCode::parse("JP").expect("static country code")),
            default_currency: Some(
                crate::domain::money::Currency::parse("JPY").expect("static currency"),
            ),
            status_hint,
        })
    }

    fn next_id(&self, prefix: &str) -> String {
        let n = self.ids.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}_{n:04}")
    }
}

#[async_trait]
impl PaymentProviderPort for FakeProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Fake
    }

    fn descriptor(&self) -> ProviderDescriptor {
        Self::descriptor()
    }

    async fn create_account(
        &self,
        request: &CreateAccountRequest,
    ) -> Result<ProviderAccount, PaymentsError> {
        let mut state = self.state.lock().expect("fake provider state lock");
        // Provider-side idempotency: the same key returns the same
        // account — the property the orphan-adoption path relies on.
        if let Some(existing) = state.by_idempotency_key.get(&request.idempotency_key) {
            return Ok(ProviderAccount {
                external_account_id: existing.clone(),
            });
        }
        let external_account_id = self.next_id("acct_fake");
        // Fresh accounts can take payments only after onboarding — the
        // same shape as a real provider, so the Active invariant is
        // exercised, not bypassed.
        let capabilities = CapabilitySet::empty().with("card_payments", CapabilityStatus::Pending);
        let requirements = if state.script.required_fields.is_empty() {
            RequirementSummary {
                currently_due: vec!["business_profile.url".to_string()],
                eventually_due: vec!["verification.document".to_string()],
                deadline: Some(Utc::now() + Duration::days(7)),
                ..RequirementSummary::default()
            }
        } else {
            RequirementSummary {
                currently_due: state.script.required_fields.clone(),
                ..RequirementSummary::default()
            }
        };
        let _ = (
            &request.country,
            &request.default_currency,
            &request.display_name,
        );
        state.accounts.insert(
            external_account_id.clone(),
            FakeAccount {
                external_account_id: external_account_id.clone(),
                capabilities,
                requirements,
                onboarding_submitted: false,
            },
        );
        state
            .by_idempotency_key
            .insert(request.idempotency_key, external_account_id.clone());
        Ok(ProviderAccount {
            external_account_id,
        })
    }

    async fn create_account_session(
        &self,
        external_account_id: &str,
    ) -> Result<ProviderAccountSession, PaymentsError> {
        let expired = {
            let state = self.state.lock().expect("fake provider state lock");
            if !state.accounts.contains_key(external_account_id) {
                return Err(PaymentsError::ProviderUnavailable(format!(
                    "fake provider knows no account {external_account_id:?}"
                )));
            }
            state.script.next_session_expired
        };
        Ok(ProviderAccountSession {
            external_account_id: external_account_id.to_string(),
            client_secret: format!("fakesession_{}", self.next_id("s")),
            expires_at: if expired {
                Utc::now() - Duration::minutes(1)
            } else {
                Utc::now() + Duration::hours(1)
            },
        })
    }

    async fn fetch_status(
        &self,
        external_account_id: &str,
    ) -> Result<ProviderAccountStatus, PaymentsError> {
        self.snapshot(external_account_id, "fetch_status")
    }

    async fn sync(
        &self,
        external_account_id: &str,
    ) -> Result<ProviderAccountStatus, PaymentsError> {
        let snapshot = self.snapshot(external_account_id, "sync")?;
        // The scripted card_payments capability, once consumed, becomes
        // the account's state (so a subsequent unscripted fetch is
        // stable).
        let scripted = {
            let state = self.state.lock().expect("fake provider state lock");
            state.script.card_payments
        };
        if let Some(status) = scripted {
            let mut state = self.state.lock().expect("fake provider state lock");
            if let Some(account) = state.accounts.get_mut(external_account_id) {
                account.capabilities = CapabilitySet::empty().with("card_payments", status);
            }
        }
        Ok(snapshot)
    }

    async fn create_checkout(
        &self,
        command: &CreateCheckoutCommand,
    ) -> Result<ProviderCheckoutSession, PaymentsError> {
        let mut state = self.state.lock().expect("fake provider state lock");
        if !state.accounts.contains_key(&command.external_account_id) {
            return Err(PaymentsError::ProviderUnavailable(format!(
                "fake provider knows no account {:?}",
                command.external_account_id
            )));
        }
        let external_session_id = self.next_id("cs_fake");
        let url = format!(
            "https://pay.fake.sanvi.dev/{}/{}/{}",
            external_session_id,
            checkout_locale(command),
            command.checkout.id.simple()
        );
        // The application fee rides only when a policy supplies one; the
        // fake records the fact so tests can assert the wiring end to end.
        tracing::debug!(
            checkout_id = %command.checkout.id,
            application_fee = ?command.application_fee.as_ref().map(|fee| fee.amount_minor),
            "fake provider checkout session created"
        );
        let session = ProviderCheckoutSession {
            external_session_id: external_session_id.clone(),
            url,
            expires_at: command
                .checkout
                .expires_at
                .or_else(|| Some(Utc::now() + Duration::minutes(30))),
        };
        state.sessions.insert(external_session_id, session.clone());
        Ok(session)
    }

    /// The replay path: same session, same URL — retrieval, not creation.
    async fn retrieve_checkout(
        &self,
        external_account_id: &str,
        external_session_id: &str,
    ) -> Result<ProviderCheckoutSession, PaymentsError> {
        let state = self.state.lock().expect("fake provider state lock");
        if !state.accounts.contains_key(external_account_id) {
            return Err(PaymentsError::ProviderUnavailable(format!(
                "fake provider knows no account {external_account_id:?}"
            )));
        }
        state
            .sessions
            .get(external_session_id)
            .cloned()
            .ok_or_else(|| {
                PaymentsError::ProviderUnavailable(format!(
                    "fake provider knows no checkout session {external_session_id:?}"
                ))
            })
    }

    async fn refund(
        &self,
        command: &ProviderRefundCommand,
    ) -> Result<ProviderRefund, PaymentsError> {
        let mut state = self.state.lock().expect("fake provider state lock");
        if !state.accounts.contains_key(&command.external_account_id) {
            return Err(PaymentsError::ProviderUnavailable(format!(
                "fake provider knows no account {:?}",
                command.external_account_id
            )));
        }
        // Provider-side idempotency, like Stripe: a key that already
        // produced a refund replays it — the re-drive path in the refund
        // use case depends on this to be safe after a lost response.
        if let Some(existing) = state.refunds_by_key.get(&command.idempotency_key) {
            return Ok(existing.clone());
        }
        // Every attempt reaches the provider (even a scripted rejection)
        // — exactly what Stripe bills an idempotency-keyed call against.
        state.refund_executions += 1;
        if let Some(reason) = state.script.fail_next_refund.take() {
            return Err(PaymentsError::ProviderUnavailable(format!(
                "scripted refund failure: {reason}"
            )));
        }
        let refund = ProviderRefund {
            external_refund_id: self.next_id("re_fake"),
            status: RefundStatus::Succeeded,
            amount: command.refund.amount.clone(),
        };
        state
            .refunds_by_key
            .insert(command.idempotency_key, refund.clone());
        Ok(refund)
    }

    async fn list_payouts(
        &self,
        external_account_id: &str,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<ProviderPayout>, PaymentsError> {
        let state = self.state.lock().expect("fake provider state lock");
        if !state.accounts.contains_key(external_account_id) {
            return Err(PaymentsError::ProviderUnavailable(format!(
                "fake provider knows no account {external_account_id:?}"
            )));
        }
        // One deterministic paid payout, filtered by the window.
        let paid_at = Utc::now() - Duration::days(1);
        if since.is_some_and(|since| paid_at < since) {
            return Ok(vec![]);
        }
        Ok(vec![ProviderPayout {
            external_payout_id: self.next_id("po_fake"),
            amount: Money::new(1500, "JPY")?,
            status: PayoutStatus::Paid,
            arrival_at: Some(paid_at),
        }])
    }

    fn verify_webhook(&self, payload: &[u8], signature_header: &str) -> Result<(), PaymentsError> {
        crate::domain::webhook::verify_webhook_signature(
            FAKE_WEBHOOK_SECRET.as_bytes(),
            payload,
            signature_header,
            300,
        )
        .map_err(Into::into)
    }
}

fn checkout_locale(command: &CreateCheckoutCommand) -> String {
    let locale = command.locale.trim().to_ascii_lowercase();
    if locale.is_empty() {
        "en".to_string()
    } else {
        locale
    }
}

#[async_trait]
impl PayoutReadPort for FakeProvider {
    async fn list_payouts(
        &self,
        external_account_id: &str,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<ProviderPayout>, PaymentsError> {
        PaymentProviderPort::list_payouts(self, external_account_id, since).await
    }
}

#[async_trait]
impl WebhookVerifierPort for FakeProvider {
    async fn verify(&self, payload: &[u8], signature_header: &str) -> Result<(), PaymentsError> {
        PaymentProviderPort::verify_webhook(self, payload, signature_header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::checkout::{Checkout, MerchantOrderRef};
    use uuid::Uuid;

    type HmacSha256 = hmac::Hmac<sha2::Sha256>;

    fn create_checkout_command(
        external_account_id: &str,
        amount_minor: i64,
        currency: &str,
    ) -> CreateCheckoutCommand {
        CreateCheckoutCommand {
            external_account_id: external_account_id.to_string(),
            checkout: Checkout::new(
                Uuid::now_v7(),
                Uuid::now_v7(),
                Uuid::now_v7(),
                MerchantOrderRef::parse("order-1").unwrap(),
                Money::new(amount_minor, currency).unwrap(),
                Utc::now(),
            )
            .unwrap(),
            success_url: "https://shop.example.test/thanks".to_string(),
            cancel_url: "https://shop.example.test/cart".to_string(),
            locale: "ja".to_string(),
            application_fee: None,
        }
    }

    // ------------------------------------------------------------------
    // The phase's acceptance criterion: the fake passes a full
    // connect → active → checkout → refund script, proving the ports are
    // enough for every later slice to build against.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn the_fake_provider_passes_the_full_lifecycle_script() {
        let provider = FakeProvider::new();
        let tenant_id = Uuid::now_v7();

        // Connect: an account exists, but cannot go active yet.
        let account = provider
            .create_account(&CreateAccountRequest {
                tenant_id,
                country: Some(CountryCode::parse("JP").unwrap()),
                default_currency: Some(crate::domain::money::Currency::parse("JPY").unwrap()),
                display_name: Some("Test Tenant".to_string()),
                idempotency_key: Uuid::now_v7(),
            })
            .await
            .unwrap();
        let early = provider.sync(&account.external_account_id).await.unwrap();
        assert_eq!(early.status_hint, Some(ConnectionStatus::Onboarding));
        assert!(!early.capabilities.is_card_payments_active());
        assert!(!early.requirements.is_empty());

        // An account session exists for the embedded onboarding surface.
        let session = provider
            .create_account_session(&account.external_account_id)
            .await
            .unwrap();
        assert!(session.client_secret.starts_with("fakesession_"));

        // Onboarding completes; the next sync reports an activatable account.
        provider.approve_onboarding(&account.external_account_id);
        let ready = provider.sync(&account.external_account_id).await.unwrap();
        assert_eq!(ready.status_hint, Some(ConnectionStatus::Active));
        assert!(ready.capabilities.is_card_payments_active());

        // Checkout on the connected account — JPY, minor units end to end.
        let command = create_checkout_command(&account.external_account_id, 2000, "JPY");
        let provider_session = provider.create_checkout(&command).await.unwrap();
        assert!(provider_session.external_session_id.starts_with("cs_fake_"));
        assert!(provider_session.url.contains("/ja"));

        // A refund of ¥500 against the ¥2,000 payment succeeds.
        let payment = crate::domain::payment::PaymentRecord::new(
            Uuid::now_v7(),
            tenant_id,
            Some(command.checkout.id),
            "pi_fake_1".to_string(),
            Money::new(2000, "JPY").unwrap(),
        )
        .unwrap();
        let refund_record = crate::domain::payment::RefundRecord::against(
            Uuid::now_v7(),
            &payment,
            500,
            None,
            None,
            Utc::now(),
        )
        .unwrap();
        let refund = provider
            .refund(&ProviderRefundCommand {
                external_account_id: account.external_account_id.clone(),
                external_payment_id: payment.external_payment_id.clone(),
                refund: refund_record,
                idempotency_key: Uuid::now_v7(),
            })
            .await
            .unwrap();
        assert_eq!(refund.status, RefundStatus::Succeeded);
        assert_eq!(refund.amount.amount_minor, 500);

        // Payout visibility: one paid payout, read-only — through the
        // payout read port, as the 09.6 dashboard will.
        let payouts = crate::domain::ports::PayoutReadPort::list_payouts(
            &provider,
            &account.external_account_id,
            None,
        )
        .await
        .unwrap();
        assert_eq!(payouts.len(), 1);
        assert_eq!(payouts[0].status, PayoutStatus::Paid);

        // Webhook verification: the fake secret verifies, others never do.
        let payload = br#"{"id": "evt_fake_1"}"#;
        let header = {
            use hmac::Mac;
            let mut mac =
                <HmacSha256 as hmac::Mac>::new_from_slice(FAKE_WEBHOOK_SECRET.as_bytes()).unwrap();
            let timestamp = Utc::now().timestamp();
            mac.update(format!("{timestamp}.{}", String::from_utf8_lossy(payload)).as_bytes());
            format!(
                "t={timestamp},v1={}",
                hex::encode(mac.finalize().into_bytes())
            )
        };
        provider.verify_webhook(payload, &header).unwrap();
        assert!(provider.verify_webhook(payload, "t=1,v1=deadbeef").is_err());
    }

    #[tokio::test]
    async fn the_fake_provider_is_scriptable_for_failure_and_surprise() {
        let provider = FakeProvider::new();
        let account = provider
            .create_account(&CreateAccountRequest {
                tenant_id: Uuid::now_v7(),
                country: None,
                default_currency: None,
                display_name: None,
                idempotency_key: Uuid::now_v7(),
            })
            .await
            .unwrap();
        provider.approve_onboarding(&account.external_account_id);

        // Scripted restriction: the sync reports Restricted with requirements.
        provider.script(FakeScript {
            next_status_override: Some(ConnectionStatus::Restricted),
            required_fields: vec!["verification.document".to_string()],
            ..FakeScript::default()
        });
        let snapshot = provider.sync(&account.external_account_id).await.unwrap();
        assert_eq!(snapshot.status_hint, Some(ConnectionStatus::Restricted));
        assert_eq!(
            snapshot.requirements.currently_due,
            ["verification.document"]
        );

        // Scripted refund failure.
        provider.script(FakeScript {
            fail_next_refund: Some("insufficient_balance".to_string()),
            ..FakeScript::default()
        });
        let payment = crate::domain::payment::PaymentRecord::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            None,
            "pi_fake_2".to_string(),
            Money::new(1000, "JPY").unwrap(),
        )
        .unwrap();
        let refund_record = crate::domain::payment::RefundRecord::against(
            Uuid::now_v7(),
            &payment,
            100,
            None,
            None,
            Utc::now(),
        )
        .unwrap();
        let err = provider
            .refund(&ProviderRefundCommand {
                external_account_id: account.external_account_id.clone(),
                external_payment_id: payment.external_payment_id.clone(),
                refund: refund_record,
                idempotency_key: Uuid::now_v7(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, PaymentsError::ProviderUnavailable(_)));
        // The scripted failure was consumed: the next refund succeeds.
        let refund_record = crate::domain::payment::RefundRecord::against(
            Uuid::now_v7(),
            &payment,
            100,
            None,
            None,
            Utc::now(),
        )
        .unwrap();
        assert!(
            provider
                .refund(&ProviderRefundCommand {
                    external_account_id: account.external_account_id.clone(),
                    external_payment_id: payment.external_payment_id,
                    refund: refund_record,
                    idempotency_key: Uuid::now_v7(),
                })
                .await
                .is_ok()
        );

        // Unknown accounts are refused, not created by side effect.
        assert!(provider.fetch_status("acct_fake_999").await.is_err());
    }

    #[test]
    fn the_descriptor_declares_itself_non_production() {
        let descriptor = FakeProvider::descriptor();
        assert_eq!(descriptor.kind, ProviderKind::Fake);
        assert!(!descriptor.production);
        assert!(descriptor.requires_onboarding);
        assert!(
            descriptor
                .supported_countries
                .iter()
                .any(|c| c.as_str() == "JP")
        );
    }
}
