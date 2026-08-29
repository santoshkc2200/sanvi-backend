//! `POST /api/v1/tenant/domains/orders` — the purchase saga.
//!
//! Charge → register → configure → active, with compensations:
//!
//! * charge fails → order `failed` (nothing to refund);
//! * registration fails after a successful charge → refund → `refunded`;
//! * refund itself fails → order `failed` with `refund_error` recorded —
//!   the order sweep retries the refund until it lands;
//! * DNS application fails → `configuring`; the order sweep retries
//!   idempotently (registration never repeats: the registrar call is
//!   keyed on the order id).
//!
//! Registrations are non-refundable once accepted — disclosed before
//! purchase; after that point failures are retried, never reversed.

use std::sync::Arc;

use rand::RngCore;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use sanvi_kernel::{RawIdGenerator, TenantId, UserId};
use sanvi_persistence::{ScopedTx, TxManager};

use crate::application::instructions::{challenge_record, routing_records};
use crate::application::views::OrderView;
use crate::domain::{
    Clock, CustomDomain, CustomDomainId, CustomDomainRepository, DnsProviderPort, DomainErrorCode,
    DomainFeatureGate, DomainKind, DomainOrder, DomainOrderId, DomainOrderRepository, DomainRole,
    DomainsError, DomainsEvent, EventPublisher, Hostname, PurchaseCharger, RegistrantContact,
    storage,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PlaceDomainOrderCommand {
    /// The hostname to purchase (must be an apex: `acme.com`).
    pub hostname: String,
    /// Registration term in years.
    #[serde(default = "default_term")]
    pub term_years: u8,
    /// Renew automatically before expiry (default true).
    #[serde(default = "default_auto_renew")]
    pub auto_renew: bool,
    /// WHOIS privacy (default true where supported).
    #[serde(default = "default_privacy")]
    pub whois_privacy: bool,
    pub registrant_contact: RegistrantContact,
    /// Client-supplied idempotency key: retrying a purchase returns the
    /// original order instead of charging twice.
    pub idempotency_key: Option<Uuid>,
}

fn default_term() -> u8 {
    1
}

fn default_auto_renew() -> bool {
    true
}

fn default_privacy() -> bool {
    true
}

pub struct PlaceDomainOrder {
    orders: Arc<dyn DomainOrderRepository>,
    domains: Arc<dyn CustomDomainRepository>,
    registrar: Arc<dyn crate::domain::DomainRegistrarPort>,
    provider: Arc<dyn DnsProviderPort>,
    charger: Arc<dyn PurchaseCharger>,
    gate: Arc<dyn DomainFeatureGate>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn RawIdGenerator>,
    tx: TxManager,
    platform_domain: String,
    edge_hostname: String,
    anycast_ips: Vec<String>,
    purchase_entitlement_key: String,
    challenge_ttl_hours: u64,
    backoff_base_secs: u64,
}

impl PlaceDomainOrder {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        orders: Arc<dyn DomainOrderRepository>,
        domains: Arc<dyn CustomDomainRepository>,
        registrar: Arc<dyn crate::domain::DomainRegistrarPort>,
        provider: Arc<dyn DnsProviderPort>,
        charger: Arc<dyn PurchaseCharger>,
        gate: Arc<dyn DomainFeatureGate>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn RawIdGenerator>,
        tx: TxManager,
        platform_domain: String,
        edge_hostname: String,
        anycast_ips: Vec<String>,
        purchase_entitlement_key: String,
        challenge_ttl_hours: u64,
        backoff_base_secs: u64,
    ) -> Self {
        Self {
            orders,
            domains,
            registrar,
            provider,
            charger,
            gate,
            events,
            clock,
            ids,
            tx,
            platform_domain,
            edge_hostname,
            anycast_ips,
            purchase_entitlement_key,
            challenge_ttl_hours,
            backoff_base_secs,
        }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        ordered_by: Option<UserId>,
        command: &PlaceDomainOrderCommand,
    ) -> Result<OrderView, DomainsError> {
        if !self
            .gate
            .is_enabled(tenant_id, &self.purchase_entitlement_key)
            .await?
        {
            return Err(DomainsError::NotEntitled {
                feature: self.purchase_entitlement_key.clone(),
            });
        }

        let hostname = Hostname::parse(&command.hostname)?;
        if !hostname.is_apex() {
            return Err(DomainsError::InvalidHostname(
                "only apex domains can be purchased (subdomains come for free)".to_string(),
            ));
        }
        if hostname.is_or_is_subdomain_of(&self.platform_domain) {
            return Err(DomainsError::HostnameOnPlatformDomain(
                command.hostname.clone(),
            ));
        }
        if command.term_years == 0 || command.term_years > 10 {
            return Err(DomainsError::Registrar(format!(
                "term_years must be 1..=10, got {}",
                command.term_years
            )));
        }

        // Idempotency: the same command returns the same order.
        if let Some(key) = command.idempotency_key {
            let mut scope = self
                .tx
                .begin_for(tenant_id.as_i64())
                .await
                .map_err(storage)?;
            if let Some(order) = self.orders.find_by_key(&mut scope, tenant_id, key).await? {
                return Ok(OrderView::of(&order));
            }
            // No match yet: the unique idempotency index on the orders
            // table is the real guard against a duplicate insert.
        }

        // Friendly uniqueness pre-check (the live-claim partial index is
        // the real guard at verification time).
        {
            let mut platform = self
                .tx
                .begin_unscoped("domains.order.live_check", "tenant")
                .await
                .map_err(storage)?;
            let live = self
                .domains
                .find_live_claim_by_hostname(&mut platform, hostname.as_str())
                .await?;
            platform.commit().await.map_err(storage)?;
            if live.is_some() {
                return Err(DomainsError::HostnameAlreadyClaimed(
                    hostname.as_str().to_string(),
                ));
            }
        }

        // Re-quote: the search price is a hint; the checkout price is a
        // promise.
        let quote = self.registrar.quote(&hostname, command.term_years).await?;
        if !quote.available {
            return Err(DomainsError::DomainUnavailable(
                hostname.as_str().to_string(),
            ));
        }

        let now = self.clock.now();
        let order_id = self.ids.next_id().map(DomainOrderId::from_snowflake)?;
        let order = DomainOrder::new(
            order_id,
            tenant_id,
            hostname.clone(),
            command.term_years,
            quote.register_price.clone(),
            command.auto_renew,
            command.whois_privacy,
            command.registrant_contact.clone(),
            ordered_by,
            command.idempotency_key,
            now,
        );

        // Step 1: record the intent (order + event in one transaction).
        {
            let mut scope = self
                .tx
                .begin_for(tenant_id.as_i64())
                .await
                .map_err(storage)?;
            self.orders.insert(&mut scope, &order).await?;
            self.events
                .publish_scoped(
                    &mut scope,
                    tenant_id,
                    &[DomainsEvent::DomainOrderPlaced {
                        order_id,
                        hostname: hostname.as_str().to_string(),
                        term_years: command.term_years,
                        price_minor: quote.register_price.minor,
                        currency: quote.register_price.currency.to_string(),
                    }],
                )
                .await?;
            scope.commit().await.map_err(storage)?;
        }

        // Step 2: charge (idempotent on the order id).
        let charge_id = match self
            .charger
            .charge(
                tenant_id,
                order_id,
                quote.register_price.minor,
                &quote.register_price.currency,
                &format!(
                    "domain registration: {} ({} years)",
                    hostname.as_str(),
                    command.term_years
                ),
            )
            .await
        {
            Ok(charge_id) => charge_id,
            Err(err) => {
                let mut order = order;
                order.mark_failed(err.to_string(), self.clock.now())?;
                self.persist_failed(&mut order, tenant_id, &err.to_string())
                    .await?;
                return Ok(OrderView::of(&order));
            }
        };

        let mut order = order;
        order.mark_charged(charge_id.clone(), self.clock.now())?;
        self.persist_order(tenant_id, &order).await?;

        // Step 3: register (idempotent on the order id).
        let registration = match self
            .registrar
            .register(
                order_id,
                &hostname,
                command.term_years,
                command.whois_privacy,
            )
            .await
        {
            Ok(registration) => registration,
            Err(err) => {
                let mut order = order;
                match self.charger.refund(tenant_id, order_id, &charge_id).await {
                    Ok(()) => {
                        order.mark_refunded(err.to_string(), self.clock.now())?;
                        self.persist_order(tenant_id, &order).await?;
                        self.events_platform(
                            tenant_id,
                            &[DomainsEvent::DomainOrderFailed {
                                order_id,
                                hostname: hostname.as_str().to_string(),
                                detail: err.to_string(),
                            }],
                        )
                        .await?;
                    }
                    Err(refund_err) => {
                        // Money unresolved: record it, the order sweep
                        // retries the refund.
                        order.mark_failed_pending_refund(
                            format!("registration failed ({err}); refund failed ({refund_err})"),
                            refund_err.to_string(),
                            self.clock.now(),
                        )?;
                        self.persist_failed_with_refund(&mut order, tenant_id)
                            .await?;
                    }
                }
                return Ok(OrderView::of(&order));
            }
        };

        order.mark_registered(
            registration.external_ref,
            registration.registered_at,
            registration.expires_at,
            self.clock.now(),
        )?;
        self.persist_order(tenant_id, &order).await?;

        // Step 4: configure DNS + create the claim. Failures leave the
        // order `configuring`; the order sweep retries.
        match self.configure_dns(tenant_id, &order, &hostname).await {
            Ok(()) => {}
            Err(err) => {
                let mut order = order;
                order.mark_configuring(self.clock.now())?;
                self.persist_order(tenant_id, &order).await?;
                tracing::warn!(
                    order_id = %order_id,
                    hostname = hostname.as_str(),
                    error = %err,
                    "domain order DNS configuration deferred to the order sweep"
                );
                return Ok(OrderView::of(&order));
            }
        }

        // DNS applied: everything downstream (verification → certificate →
        // live) is worker-driven with no human step. Re-read the order:
        // `configure_dns` advanced it in its own transaction.
        self.load_order(tenant_id, order_id).await
    }

    async fn load_order(
        &self,
        tenant_id: TenantId,
        order_id: DomainOrderId,
    ) -> Result<OrderView, DomainsError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(storage)?;
        let order = self
            .orders
            .find(&mut scope, tenant_id, order_id)
            .await?
            .ok_or(DomainsError::UnknownOrder(order_id))?;
        scope.commit().await.map_err(storage)?;
        Ok(OrderView::of(&order))
    }

    /// Apply DNS records and create the purchased claim + mark the order
    /// active, all in one transaction. Retried idempotently by the order
    /// sweep.
    pub async fn configure_dns(
        &self,
        tenant_id: TenantId,
        order: &DomainOrder,
        hostname: &Hostname,
    ) -> Result<(), DomainsError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(storage)?;

        // Re-read inside the transaction: a retry may race a success.
        let order = self
            .orders
            .find(&mut scope, tenant_id, order.id)
            .await?
            .ok_or(DomainsError::UnknownOrder(order.id))?;

        self.provider.create_zone(&mut scope, hostname).await?;

        let now = self.clock.now();
        let claim_id = self.ids.next_id().map(CustomDomainId::from_snowflake)?;
        let mut token_bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut token_bytes);
        let mut token = hex::encode(token_bytes);
        let mut role = match self.domains.find_primary(&mut scope, tenant_id).await? {
            Some(_) => DomainRole::Alias,
            None => DomainRole::Primary,
        };

        // Resolve the claim this order flows through. An existing active
        // claim for the same hostname is either this saga's own retry
        // (reuse its id and challenge token — the token is what the
        // verification sweep compares the applied TXT record against) or a
        // connect-journey claim superseded by the purchase: close it and
        // let the purchased claim take over, inheriting the primary role.
        // The hostname can never hold a *live* claim here — the order was
        // rejected up front if it did.
        let claim_id = match self
            .domains
            .find_active_by_hostname_for_tenant(&mut scope, tenant_id, hostname.as_str())
            .await?
        {
            Some(existing) if existing.kind == DomainKind::Purchased => {
                if let Some(challenge) = &existing.challenge {
                    token = challenge.token.clone();
                }
                existing.id
            }
            Some(existing) => {
                let inherits_primary = existing.role == DomainRole::Primary;
                let mut superseded = existing;
                superseded.remove(
                    DomainErrorCode::SUPERSEDED_BY_PURCHASE,
                    "closed when the tenant purchased the hostname through Sanvi".to_string(),
                    now,
                )?;
                self.domains.update(&mut scope, &superseded).await?;
                if inherits_primary {
                    role = DomainRole::Primary;
                }
                let claim = CustomDomain::claim(
                    claim_id,
                    tenant_id,
                    hostname.clone(),
                    DomainKind::Purchased,
                    role,
                    token.clone(),
                    now,
                    self.challenge_ttl_hours,
                    self.backoff_base_secs,
                );
                self.domains.insert(&mut scope, &claim).await?;
                claim.id
            }
            None => {
                let claim = CustomDomain::claim(
                    claim_id,
                    tenant_id,
                    hostname.clone(),
                    DomainKind::Purchased,
                    role,
                    token.clone(),
                    now,
                    self.challenge_ttl_hours,
                    self.backoff_base_secs,
                );
                self.domains.insert(&mut scope, &claim).await?;
                claim.id
            }
        };

        let mut records = vec![challenge_record(&token)];
        records.extend(routing_records(
            hostname,
            &self.edge_hostname,
            &self.anycast_ips,
        ));

        self.provider
            .apply_records(&mut scope, claim_id, &records)
            .await?;

        let mut order = order;
        order.mark_active(now)?;
        self.orders.update(&mut scope, &order).await?;

        self.events
            .publish_scoped(
                &mut scope,
                tenant_id,
                &[
                    DomainsEvent::DomainClaimed {
                        domain_id: claim_id,
                        hostname: hostname.as_str().to_string(),
                        domain_kind: DomainKind::Purchased.as_str().to_string(),
                        role: role.as_str().to_string(),
                    },
                    DomainsEvent::DomainOrderCompleted {
                        order_id: order.id,
                        hostname: hostname.as_str().to_string(),
                    },
                ],
            )
            .await?;

        scope.commit().await.map_err(storage)?;
        Ok(())
    }

    async fn persist_order(
        &self,
        tenant_id: TenantId,
        order: &DomainOrder,
    ) -> Result<(), DomainsError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(storage)?;
        self.orders.update(&mut scope, order).await?;
        scope.commit().await.map_err(storage)
    }

    async fn persist_failed(
        &self,
        order: &mut DomainOrder,
        tenant_id: TenantId,
        detail: &str,
    ) -> Result<(), DomainsError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(storage)?;
        self.orders.update(&mut scope, order).await?;
        self.events
            .publish_scoped(
                &mut scope,
                tenant_id,
                &[DomainsEvent::DomainOrderFailed {
                    order_id: order.id,
                    hostname: order.hostname.as_str().to_string(),
                    detail: detail.to_string(),
                }],
            )
            .await?;
        scope.commit().await.map_err(storage)
    }

    async fn persist_failed_with_refund(
        &self,
        order: &mut DomainOrder,
        tenant_id: TenantId,
    ) -> Result<(), DomainsError> {
        // Same as persist_failed but keeps `refund_error` in the failure
        // detail for the order sweep to retry.
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(storage)?;
        self.orders.update(&mut scope, order).await?;
        self.events
            .publish_scoped(
                &mut scope,
                tenant_id,
                &[DomainsEvent::DomainOrderFailed {
                    order_id: order.id,
                    hostname: order.hostname.as_str().to_string(),
                    detail: "registration failed and the refund is pending retry".to_string(),
                }],
            )
            .await?;
        scope.commit().await.map_err(storage)
    }

    async fn events_platform(
        &self,
        tenant_id: TenantId,
        events: &[DomainsEvent],
    ) -> Result<(), DomainsError> {
        let mut scope = self
            .tx
            .begin_unscoped("domains.order.events", "tenant")
            .await
            .map_err(storage)?;
        self.events.publish(&mut scope, tenant_id, events).await?;
        scope.commit().await.map_err(storage)
    }
}

/// `POST /api/v1/tenant/domains/orders/{id}/auto-renew`.
pub struct SetOrderAutoRenew {
    orders: Arc<dyn DomainOrderRepository>,
    clock: Arc<dyn Clock>,
    enabled: bool,
}

impl SetOrderAutoRenew {
    pub fn new(
        orders: Arc<dyn DomainOrderRepository>,
        clock: Arc<dyn Clock>,
        enabled: bool,
    ) -> Self {
        Self {
            orders,
            clock,
            enabled,
        }
    }

    pub async fn execute(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        order_id: DomainOrderId,
    ) -> Result<OrderView, DomainsError> {
        let mut order = self
            .orders
            .find(scope, tenant_id, order_id)
            .await?
            .ok_or(DomainsError::UnknownOrder(order_id))?;
        order.set_auto_renew(self.enabled, self.clock.now())?;
        self.orders.update(scope, &order).await?;
        Ok(OrderView::of(&order))
    }
}
