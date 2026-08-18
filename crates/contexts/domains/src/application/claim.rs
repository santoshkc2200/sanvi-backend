//! `POST /api/v1/tenant/domains` — claim a connected domain.
//!
//! Validation order matters: hostname shape, then platform-domain rules,
//! then entitlement, then uniqueness. A claimed hostname gets a
//! high-entropy challenge token and the exact DNS records the tenant must
//! create; the verification worker picks it up on its next sweep.

use std::sync::Arc;

use rand::RngCore;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use sanvi_persistence::{ScopedTx, TxManager};

use crate::application::views::CustomDomainView;
use crate::domain::{
    Clock, CustomDomain, CustomDomainRepository, DomainFeatureGate, DomainKind, DomainRole,
    DomainsError, DomainsEvent, EventPublisher, Hostname, storage,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ClaimCustomDomainCommand {
    /// The hostname to claim (`shop.acme.com`).
    pub hostname: String,
    /// `primary` (default when the tenant has none), `alias` or `redirect`.
    #[serde(default)]
    pub role: Option<String>,
}

pub struct ClaimCustomDomain {
    domains: Arc<dyn CustomDomainRepository>,
    gate: Arc<dyn DomainFeatureGate>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    tx: TxManager,
    platform_domain: String,
    connect_entitlement_key: String,
    challenge_ttl_hours: u64,
    backoff_base_secs: u64,
}

impl ClaimCustomDomain {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        domains: Arc<dyn CustomDomainRepository>,
        gate: Arc<dyn DomainFeatureGate>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        tx: TxManager,
        platform_domain: String,
        connect_entitlement_key: String,
        challenge_ttl_hours: u64,
        backoff_base_secs: u64,
    ) -> Self {
        Self {
            domains,
            gate,
            events,
            clock,
            tx,
            platform_domain,
            connect_entitlement_key,
            challenge_ttl_hours,
            backoff_base_secs,
        }
    }

    pub async fn execute(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        command: &ClaimCustomDomainCommand,
    ) -> Result<CustomDomainView, DomainsError> {
        let hostname = Hostname::parse(&command.hostname)?;
        if hostname.is_or_is_subdomain_of(&self.platform_domain) {
            return Err(DomainsError::HostnameOnPlatformDomain(
                command.hostname.clone(),
            ));
        }

        if !self
            .gate
            .is_enabled(tenant_id, &self.connect_entitlement_key)
            .await?
        {
            return Err(DomainsError::NotEntitled {
                feature: self.connect_entitlement_key.clone(),
            });
        }

        let role = match command.role.as_deref() {
            Some(raw) => DomainRole::parse(raw)?,
            None => match self.domains.find_primary(scope, tenant_id).await? {
                Some(_) => DomainRole::Alias,
                None => DomainRole::Primary,
            },
        };

        // One active claim per tenant per hostname.
        if let Some(existing) = self
            .domains
            .find_active_by_hostname_for_tenant(scope, tenant_id, hostname.as_str())
            .await?
        {
            return Err(DomainsError::HostnameAlreadyRegistered(
                existing.hostname.as_str().to_string(),
            ));
        }

        // Another tenant's live claim? Friendly pre-check in an unscoped
        // transaction (the tenant scope's RLS cannot see those rows). The
        // database partial unique index is the real guard; a race here is
        // resolved at verification time, not at claim time.
        {
            let mut platform = self
                .tx
                .begin_unscoped("domains.claim.live_check", "tenant")
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

        let now = self.clock.now();
        let mut token_bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut token_bytes);
        let token = hex::encode(token_bytes);

        let domain = CustomDomain::claim(
            Uuid::now_v7(),
            tenant_id,
            hostname.clone(),
            DomainKind::Connected,
            role,
            token,
            now,
            self.challenge_ttl_hours,
            self.backoff_base_secs,
        );
        self.domains.insert(scope, &domain).await?;
        self.events
            .publish_scoped(
                scope,
                tenant_id,
                &[DomainsEvent::DomainClaimed {
                    domain_id: domain.id,
                    hostname: hostname.as_str().to_string(),
                    domain_kind: domain.kind.as_str().to_string(),
                    role: domain.role.as_str().to_string(),
                }],
            )
            .await?;

        Ok(CustomDomainView::of(&domain))
    }
}
