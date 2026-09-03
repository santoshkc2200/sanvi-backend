//! The create-checkout use case (sub-phase 09.4): the slice where the
//! "never merchant of record" invariant is either proven or quietly
//! broken.
//!
//! The two rules that shape everything here:
//!
//! 1. **Direct charges only.** The session is created on the connected
//!    account — the use case loads the tenant's own connection through an
//!    RLS-scoped repository and passes *its* account id to the adapter,
//!    then asserts the outbound command carries exactly that id. Tenant A
//!    cannot rent tenant B's account: not by RLS alone, but structurally.
//! 2. **The webhook is the fact; the redirect is a hint.** Nothing here
//!    derives order state from a return URL; the handlers on 09.3's
//!    transport do. A customer who never returns still gets a paid order;
//!    a forged return URL gets nothing.
//!
//! Order of operations is the spec: validate everything we can before
//! touching the provider (a rejected session at Stripe is a worse error
//! message than ours), persist the checkout row **before** creating the
//! session (an interrupted request is recoverable and the idempotency key
//! has an owner), and only then call the provider keyed by our checkout
//! id so a retry after a crash adopts the same session.

use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;

use crate::domain::checkout::{Checkout, CheckoutId, MerchantOrderRef};
use crate::domain::connection::ProviderConnection;
use crate::domain::error::PaymentsError;
use crate::domain::events::{EventPublisher, PaymentsEvent};
use crate::domain::money::Money;
use crate::domain::payment::PaymentId;
use crate::domain::ports::{
    CheckoutRepository, CreateCheckoutCommand, IdGenerator, PaymentProviderPort,
    ProviderCheckoutSession, ProviderConnectionRepository, TenantDomainAllowList,
};

/// The conversion event id phase 10 consumes: derived server-side from
/// `(tenant, payment)` — once per order by construction, identical on
/// every refresh of the confirmation page, so a refreshed page cannot
/// double-count. Never stored: derivation *is* the dedupe.
pub fn conversion_event_id(_tenant_id: TenantId, payment_id: PaymentId) -> PaymentId {
    payment_id
}

/// The platform's fee policy for direct charges. Off in 0.10.0 — the
/// platform monetises via subscription — and TASK-007 owns the real
/// per-tenant configuration (platform-admin data, read through
/// [`crate::domain::ports::ApplicationFeeStore`]). While [`NoApplicationFee`]
/// is wired, no fee parameter ever leaves this process on any tenant
/// transaction — the release-default regression guard TASK-005 asserts.
#[async_trait::async_trait]
pub trait ApplicationFeePolicy: Send + Sync {
    /// The fee for one charge of this tenant, or `None`. Implementations
    /// compute minor units via [`crate::domain::fees`] — round half up on
    /// the fee, never on the total.
    async fn application_fee(&self, tenant_id: TenantId, amount: &Money) -> Option<Money>;
}

/// The shipped policy: fees are off.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoApplicationFee;

#[async_trait::async_trait]
impl ApplicationFeePolicy for NoApplicationFee {
    async fn application_fee(&self, _tenant_id: TenantId, _amount: &Money) -> Option<Money> {
        None
    }
}

/// One checkout-creation request from the storefront.
#[derive(Debug, Clone)]
pub struct NewCheckoutCommand {
    pub tenant_id: TenantId,
    /// The merchant's order reference (opaque to Sanvi, echoed by the
    /// provider as its client reference).
    pub reference: String,
    /// Minor units + ISO-4217 — ¥2,000 is `2000`, never `200000`.
    pub amount_minor: i64,
    pub currency: String,
    /// Free-form customer/order context, provider-passthrough. No card
    /// data, ever.
    pub customer_ref: Option<serde_json::Value>,
    /// Presentation locale for the hosted page (phase 06 vocabulary).
    pub locale: Option<String>,
    /// Redirect targets on the tenant's **verified** domain (phase 08).
    /// Hints only — the confirmation page trusts only `GET`.
    pub success_url: String,
    pub cancel_url: String,
    /// Required (NFR-904). A double-clicked buy button must not create
    /// two sessions.
    pub idempotency_key: Uuid,
}

/// What one execution produced: the persisted aggregate plus the hosted
/// session details when a session exists yet.
#[derive(Debug, Clone)]
pub struct CheckoutOutcome {
    pub checkout: Checkout,
    /// The provider-hosted URL (`None` only for replays of terminal or
    /// unrecoverable checkouts whose session cannot be re-fetched).
    pub url: Option<String>,
    pub expires_at: Option<chrono::DateTime<Utc>>,
    /// True when the Idempotency-Key already owned a row: the response
    /// describes the *same* logical checkout, never a second session.
    pub replayed: bool,
}

/// What the storefront needs before it renders a Pay button: whether the
/// tenant's connected account can take money right now, why not when it
/// cannot, and the currency it settles in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutReadiness {
    pub can_accept_payments: bool,
    /// An i18n key (`payments.blocker.*`), never a rendered sentence.
    pub cannot_accept_reason: Option<String>,
    /// The connected account's settlement currency; `None` when there is
    /// no usable connection to read one from.
    pub currency: Option<String>,
}

pub struct CreateCheckout {
    tx_manager: TxManager,
    registry: Arc<super::registry::ProviderRegistry>,
    connections: Arc<dyn ProviderConnectionRepository>,
    checkouts: Arc<dyn CheckoutRepository>,
    domains: Arc<dyn TenantDomainAllowList>,
    events: Arc<dyn EventPublisher>,
    fee_policy: Arc<dyn ApplicationFeePolicy>,
    tax_settings: Arc<dyn crate::domain::ports::TaxSettingsRepository>,
    ids: Arc<dyn IdGenerator>,
}

impl CreateCheckout {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx_manager: TxManager,
        registry: Arc<super::registry::ProviderRegistry>,
        connections: Arc<dyn ProviderConnectionRepository>,
        checkouts: Arc<dyn CheckoutRepository>,
        domains: Arc<dyn TenantDomainAllowList>,
        events: Arc<dyn EventPublisher>,
        fee_policy: Arc<dyn ApplicationFeePolicy>,
        tax_settings: Arc<dyn crate::domain::ports::TaxSettingsRepository>,
        ids: Arc<dyn IdGenerator>,
    ) -> Self {
        Self {
            tx_manager,
            registry,
            connections,
            checkouts,
            domains,
            events,
            fee_policy,
            tax_settings,
            ids,
        }
    }

    /// Create a checkout session for the storefront: validate → gate on
    /// the tenant's live connection → persist → call the provider. The
    /// span name is the trace's root for the buy path (09.7); the
    /// adapter's own spans nest under it, so a trace covers the outbound
    /// Stripe call end to end.
    #[tracing::instrument(
        name = "payments.checkout.create",
        skip_all,
        fields(tenant_id = %command.tenant_id, reference = %command.reference)
    )]
    pub async fn execute(
        &self,
        command: NewCheckoutCommand,
    ) -> Result<CheckoutOutcome, PaymentsError> {
        // ---- pure validation, before any I/O --------------------------
        let reference = MerchantOrderRef::parse(&command.reference)?;
        let amount = Money::new(command.amount_minor, &command.currency)?;
        let locale = validated_locale(command.locale.as_deref())?;
        https_hostname(&command.success_url)?;
        https_hostname(&command.cancel_url)?;

        // ---- the connection gate (RLS-scoped) -------------------------
        let connection = self.active_connection(command.tenant_id).await?;

        // Pre-flight currency agreement: refuse with *our* error before
        // Stripe can refuse with theirs.
        if let Some(default_currency) = &connection.default_currency
            && default_currency != amount.currency.as_str()
        {
            return Err(PaymentsError::CurrencyUnsupported {
                currency: amount.currency.as_str().to_string(),
                detail: format!(
                    "the connected account settles in {default_currency} (country {})",
                    connection.country.as_deref().unwrap_or("unknown")
                ),
            });
        }

        // The verified-domain allow-list. Allow-list, never sanitize: a
        // URL is either on one of *this tenant's* live domains or the
        // request dies here — an open redirect is a phishing primitive.
        self.guard_domain(command.tenant_id, &https_hostname(&command.success_url)?)
            .await?;
        self.guard_domain(command.tenant_id, &https_hostname(&command.cancel_url)?)
            .await?;

        // ---- idempotent owner lookup ----------------------------------
        if let Some(existing) = self
            .find_by_key(command.tenant_id, command.idempotency_key)
            .await?
        {
            // The key already owns a row: resume *it*, whatever state the
            // interrupted first attempt left behind.
            return self.resume(existing, &connection, &command).await;
        }

        // ---- persist BEFORE the provider call -------------------------
        let checkout_id = self.ids.next_checkout_id()?;
        let now = Utc::now();
        let mut checkout = Checkout::new(
            checkout_id,
            command.tenant_id,
            connection.id,
            reference,
            amount,
            now,
        )?;
        checkout.customer_ref.clone_from(&command.customer_ref);
        checkout.locale.clone_from(&locale);
        checkout.idempotency_key = Some(command.idempotency_key);

        match self.persist_new(&connection.provider, &checkout).await {
            Ok(()) => {}
            Err(PaymentsError::IdempotencyConflict(_)) => {
                // A concurrent request with the same key won the insert:
                // adopt its row instead of failing the double-click.
                if let Some(adopted) = self
                    .find_by_key(command.tenant_id, command.idempotency_key)
                    .await?
                {
                    return self.resume(adopted, &connection, &command).await;
                }
                return Err(PaymentsError::Storage(
                    "checkout insert raced and no adopting row was found".to_string(),
                ));
            }
            Err(err) => return Err(err),
        }

        let session = self
            .open_session(&connection, &mut checkout, &command)
            .await?;
        Ok(CheckoutOutcome {
            expires_at: session.expires_at,
            url: Some(session.url),
            checkout,
            replayed: false,
        })
    }

    /// An idempotency-key hit. Terminal states answer straight from the
    /// record; a pending row (crash between persistence and session
    /// creation) re-drives the provider call under the same derived id;
    /// an open row re-fetches its live session so the replay carries the
    /// same hosted URL instead of minting a second one.
    async fn resume(
        &self,
        mut checkout: Checkout,
        connection: &ProviderConnection,
        command: &NewCheckoutCommand,
    ) -> Result<CheckoutOutcome, PaymentsError> {
        if checkout.external_session_id.is_none() && checkout.status.is_live() {
            // Crash window: the row owns the key but no session exists.
            // The retrying request supplies the redirect targets again.
            checkout.customer_ref.clone_from(&command.customer_ref);
            let session = self
                .open_session(connection, &mut checkout, command)
                .await?;
            return Ok(CheckoutOutcome {
                expires_at: session.expires_at,
                url: Some(session.url),
                checkout,
                replayed: true,
            });
        }

        if !checkout.status.is_live() {
            return Ok(CheckoutOutcome {
                url: None,
                expires_at: checkout.expires_at,
                checkout,
                replayed: true,
            });
        }

        // Live session: refresh url/expiry without creating anything.
        let adapter = self.adapter(&connection.provider)?;
        let session_id = checkout.external_session_id.clone().ok_or_else(|| {
            PaymentsError::Storage("live checkout carries no session id".to_string())
        })?;
        let url = match adapter
            .retrieve_checkout(&connection.external_account_id, &session_id)
            .await
        {
            Ok(session) => {
                checkout.expires_at = session.expires_at.or(checkout.expires_at);
                Some(session.url)
            }
            Err(err) => {
                tracing::warn!(
                    checkout_id = %checkout.id,
                    error = %err,
                    "checkout replay could not re-fetch its session; answering from the record"
                );
                None
            }
        };
        Ok(CheckoutOutcome {
            url,
            expires_at: checkout.expires_at,
            checkout,
            replayed: true,
        })
    }

    /// The storefront's pre-flight answer to "can this tenant be paid,
    /// and in what currency" — the *same* gate `execute` applies, asked
    /// before a buyer commits to anything. Without it the Pay button can
    /// only find out by being clicked and eating the refusal.
    pub async fn readiness(&self, tenant_id: TenantId) -> Result<CheckoutReadiness, PaymentsError> {
        match self.active_connection(tenant_id).await {
            Ok(connection) => Ok(CheckoutReadiness {
                can_accept_payments: true,
                cannot_accept_reason: None,
                currency: connection.default_currency.clone(),
            }),
            // The refusal key is the whole point of the call: it is what
            // the storefront renders next to a disabled button.
            Err(PaymentsError::ConnectionNotActive(reason)) => Ok(CheckoutReadiness {
                can_accept_payments: false,
                cannot_accept_reason: Some(reason),
                currency: None,
            }),
            Err(err) => Err(err),
        }
    }

    /// Create the provider session on the connected account and move the
    /// row to `Open` — with the outbound isolation guard asserting the
    /// command carries *this* tenant's account id.
    async fn open_session(
        &self,
        connection: &ProviderConnection,
        checkout: &mut Checkout,
        command: &NewCheckoutCommand,
    ) -> Result<ProviderCheckoutSession, PaymentsError> {
        let adapter = self.adapter(&connection.provider)?;
        let application_fee = self
            .fee_policy
            .application_fee(checkout.tenant_id, &checkout.amount)
            .await;
        // Automatic tax rides only when the tenant's stored settings say
        // so — and those settings can be enabled only through a passed
        // compliance preflight, so a session created here carries the
        // liability of the connected account, the merchant of record.
        let automatic_tax = self
            .load_tax_liability(checkout.tenant_id, &connection.external_account_id)
            .await?;
        let addressed = CreateCheckoutCommand {
            external_account_id: connection.external_account_id.clone(),
            checkout: checkout.clone(),
            success_url: success_url_with_checkout_id(&command.success_url, checkout.id),
            cancel_url: command.cancel_url.clone(),
            locale: checkout.locale.clone().unwrap_or_default(),
            application_fee,
            automatic_tax,
        };

        // The outbound isolation guard (the task's explicit assertion):
        // RLS protected the rows above; this protects the API call. The
        // account header the adapter sends is built *only* from this
        // field, and the field was taken from the requesting tenant's own
        // loaded connection — the assert documents the invariant and
        // catches any future refactor that would break it.
        assert_eq!(
            addressed.external_account_id, connection.external_account_id,
            "checkout command must target the requesting tenant's connected account"
        );

        let session = adapter.create_checkout(&addressed).await?;
        checkout.session_created(session.external_session_id.clone(), session.expires_at)?;
        self.persist_update(checkout).await?;
        Ok(session)
    }

    /// The tax liability for this tenant's sessions, when their settings
    /// are enabled. The stored liability account must equal the live
    /// connection's account id — after a reconnection (new account) the
    /// stale liability is not sent; the next preflight refreshes it.
    async fn load_tax_liability(
        &self,
        tenant_id: TenantId,
        live_account_id: &str,
    ) -> Result<Option<crate::domain::ports::AutomaticTaxLiability>, PaymentsError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("load_tax_settings_for_checkout", "payments-checkout")
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        let record = self
            .tax_settings
            .find_by_tenant(&mut scope, tenant_id)
            .await;
        let _ = scope.rollback().await;
        Ok(record?
            .filter(|settings| settings.enabled && settings.liability_account == live_account_id)
            .map(|settings| crate::domain::ports::AutomaticTaxLiability {
                account: settings.liability_account,
            }))
    }

    async fn persist_new(
        &self,
        provider: &crate::domain::ProviderKind,
        checkout: &Checkout,
    ) -> Result<(), PaymentsError> {
        let tenant_id = checkout.tenant_id;
        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        if let Err(err) = self.checkouts.insert(&mut scope, checkout).await {
            let _ = scope.rollback().await;
            return Err(err);
        }
        self.events
            .publish_scoped(
                &mut scope,
                tenant_id,
                &[PaymentsEvent::CheckoutCreated {
                    checkout_id: checkout.id,
                    connection_id: checkout.connection_id,
                    provider: provider.as_str().to_string(),
                    reference: checkout.reference.as_str().to_string(),
                    amount: crate::domain::events::EventAmount::new(&checkout.amount),
                }],
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        Ok(())
    }

    async fn persist_update(&self, checkout: &Checkout) -> Result<(), PaymentsError> {
        let tenant_id = checkout.tenant_id;
        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        self.checkouts.update(&mut scope, checkout).await?;
        scope
            .commit()
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        Ok(())
    }

    async fn find_by_key(
        &self,
        tenant_id: TenantId,
        idempotency_key: Uuid,
    ) -> Result<Option<Checkout>, PaymentsError> {
        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        let found = self
            .checkouts
            .find_by_idempotency_key(&mut scope, tenant_id, idempotency_key)
            .await;
        let _ = scope.rollback().await;
        found
    }

    /// The tenant's live, payments-capable connection — refused with a
    /// reason code the UI can render otherwise (`can_accept_payments`,
    /// the TASK-004 computed field, is exactly this gate).
    async fn active_connection(
        &self,
        tenant_id: TenantId,
    ) -> Result<ProviderConnection, PaymentsError> {
        let refused = |key: &str| PaymentsError::ConnectionNotActive(key.to_string());
        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        let connections = self.connections.list(&mut scope, tenant_id).await;
        let _ = scope.rollback().await;

        // Newest first; skip history rows. No live row at all is the
        // "onboarding never started" blocker.
        let Some(latest) = connections?
            .into_iter()
            .find(|c| c.disconnected_at.is_none())
        else {
            return Err(refused("payments.blocker.onboarding_not_started"));
        };
        if latest.status == crate::domain::ConnectionStatus::Pending {
            return Err(refused("payments.blocker.onboarding_not_started"));
        }
        if !latest.capability_set().is_card_payments_active() {
            return Err(refused("payments.blocker.card_payments_inactive"));
        }
        if !latest.requirement_summary().past_due.is_empty() {
            return Err(refused("payments.blocker.requirements_past_due"));
        }
        Ok(latest)
    }

    async fn guard_domain(&self, tenant_id: TenantId, host: &str) -> Result<(), PaymentsError> {
        if !self.domains.allows(tenant_id, host).await? {
            return Err(PaymentsError::DomainNotAllowed(host.to_string()));
        }
        Ok(())
    }

    fn adapter(
        &self,
        provider: &crate::domain::ProviderKind,
    ) -> Result<Arc<dyn PaymentProviderPort>, PaymentsError> {
        self.registry.get(*provider).ok_or_else(|| {
            PaymentsError::InvalidConnection(format!(
                "provider {:?} is not available in this environment",
                provider.as_str()
            ))
        })
    }
}

/// Extract and vet the hostname of a redirect target. HTTPS only, no
/// userinfo, no non-default port, no IP literals — anything else is an
/// error before the allow-list is even consulted. The allow-list remains
/// the authority; this parsing just makes sure we compare *a hostname*.
/// Carry the checkout id back on the success URL.
///
/// The confirmation page is otherwise left resolving the checkout from
/// tab-scoped session storage, so a return into a different tab — or with
/// storage unavailable — cannot find a checkout the buyer has just paid
/// for. Appended *after* `guard_domain` has ruled on the host: the
/// authority is untouched here, so the verified-domain allow-list remains
/// the only thing that decides where a buyer can be sent.
fn success_url_with_checkout_id(raw: &str, checkout_id: CheckoutId) -> String {
    let (before_fragment, fragment) = match raw.split_once('#') {
        Some((head, frag)) => (head, Some(frag)),
        None => (raw, None),
    };
    let separator = if before_fragment.contains('?') {
        '&'
    } else {
        '?'
    };
    let mut out = format!("{before_fragment}{separator}checkout_id={checkout_id}");
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

fn https_hostname(raw: &str) -> Result<String, PaymentsError> {
    const MAX_URL_LEN: usize = 2048;
    if raw.len() > MAX_URL_LEN {
        return Err(PaymentsError::DomainNotAllowed(
            "redirect URL exceeds 2048 characters".to_string(),
        ));
    }
    let rest = raw
        .strip_prefix("https://")
        .ok_or_else(|| domain_err("redirect URLs must be absolute https:// URLs"))?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return Err(domain_err("redirect URL has no host"));
    }
    if authority.contains('@') {
        return Err(domain_err("redirect URL must not carry userinfo"));
    }
    // Strip an explicit default port; any other port is refused (verified
    // domains are DNS names served over TLS).
    let host = match authority.rsplit_once(':') {
        Some((host, port)) => {
            if port != "443" || host.contains(':') {
                return Err(domain_err("redirect URL must use the default https port"));
            }
            host
        }
        None => authority,
    };
    if host.starts_with('[') || host.contains(':') {
        return Err(domain_err("redirect URL host must be a DNS name"));
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty()
        || host.parse::<std::net::IpAddr>().is_ok()
        || !host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
    {
        return Err(domain_err("redirect URL host must be a DNS name"));
    }
    Ok(host)
}

fn domain_err(detail: &str) -> PaymentsError {
    PaymentsError::DomainNotAllowed(detail.to_string())
}

/// Validate the presentation locale passed to the provider's hosted page:
/// phase-06 style tags (`ja`, `en-US`), bounded, alphanumeric-plus-dash —
/// never free-form strings riding into provider parameters.
fn validated_locale(raw: Option<&str>) -> Result<Option<String>, PaymentsError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.len() > 35
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(PaymentsError::InvalidConnection(format!(
            "locale {trimmed:?} is not a bounded language tag"
        )));
    }
    Ok(Some(trimmed.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- pure validation ----------------------------------------------------

    #[test]
    fn redirect_targets_must_be_absolute_https_dns_urls() {
        assert_eq!(
            https_hostname("https://shop.example.test/thanks?a=1#frag")
                .unwrap()
                .as_str(),
            "shop.example.test"
        );
        // Default-port explicit form normalizes onto the bare host.
        assert_eq!(
            https_hostname("https://shop.example.test:443/ok")
                .unwrap()
                .as_str(),
            "shop.example.test"
        );
        // Trailing dot is normalized away.
        assert_eq!(
            https_hostname("https://SHOP.Example.test./x")
                .unwrap()
                .as_str(),
            "shop.example.test"
        );

        // Plaintext http is refused outright.
        assert!(https_hostname("http://shop.example.test/thanks").is_err());
        // Relative URLs are refused (no implicit origin).
        assert!(https_hostname("/confirmation").is_err());
        // Userinfo smuggling (`https://evil.test@good.test/`) dies here.
        assert!(https_hostname("https://evil.test@shop.example.test/x").is_err());
        // Non-default ports are refused.
        assert!(https_hostname("https://shop.example.test:8443/x").is_err());
        // IP-literal hosts can never be verified domains.
        assert!(https_hostname("https://203.0.113.9/thanks").is_err());
        assert!(https_hostname("https://[2001:db8::1]/thanks").is_err());
        // Garbage hosts die at the character gate.
        assert!(https_hostname("https://under_score.test/x").is_err());
        assert!(https_hostname("").is_err());
    }

    #[test]
    fn locales_are_bounded_language_tags() {
        assert_eq!(
            validated_locale(Some(" ja-JP ")).unwrap().as_deref(),
            Some("ja-jp")
        );
        assert_eq!(validated_locale(None).unwrap(), None);
        assert_eq!(validated_locale(Some("  ")).unwrap(), None);
        assert!(validated_locale(Some("ja JP")).is_err());
        assert!(validated_locale(Some(&"x".repeat(36))).is_err());
    }

    #[tokio::test]
    async fn the_shipped_fee_policy_is_off() {
        // While fees are off no amount shape can conjure a fee.
        assert_eq!(
            NoApplicationFee
                .application_fee(
                    TenantId::try_from(100).unwrap(),
                    &Money::new(2000, "JPY").unwrap()
                )
                .await,
            None
        );
    }
}
