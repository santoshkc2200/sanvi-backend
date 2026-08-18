//! The TLS authorize gate: the single question the edge proxy asks before
//! issuing a certificate on demand.
//!
//! `GET /internal/tls/authorize?host=` returns 200 only for a hostname in
//! the live-hostname projection (verified/issuing_cert/live/degraded by
//! construction — the projection is maintained transactionally with the
//! state machine) belonging to an `active` tenant. Everything else denies —
//! that is what stops abuse-driven issuance at the ACME account.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::{CustomDomainRepository, DomainsError, Hostname, TenantState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizeDecision {
    Allowed,
    DeniedUnknownHost,
    DeniedNotVerified,
    DeniedTenantInactive,
}

impl AuthorizeDecision {
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed)
    }

    pub const fn reason(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::DeniedUnknownHost => "denied_unknown_host",
            Self::DeniedNotVerified => "denied_not_verified",
            Self::DeniedTenantInactive => "denied_tenant_inactive",
        }
    }
}

pub struct AuthorizeTls {
    domains: Arc<dyn CustomDomainRepository>,
    tenants: Arc<dyn TenantState>,
    pool: sanvi_persistence::PgPool,
}

impl AuthorizeTls {
    pub fn new(
        domains: Arc<dyn CustomDomainRepository>,
        tenants: Arc<dyn TenantState>,
        pool: sanvi_persistence::PgPool,
    ) -> Self {
        Self {
            domains,
            tenants,
            pool,
        }
    }

    pub async fn execute(&self, host: &str) -> Result<AuthorizeDecision, DomainsError> {
        // Host headers can carry a port (`example.com:443`); normalize it
        // away before parsing. Anything unparseable is simply not served.
        let host = host.rsplit_once(':').map(|(name, _)| name).unwrap_or(host);
        let hostname = match Hostname::parse(host) {
            Ok(hostname) => hostname,
            Err(_) => return Ok(AuthorizeDecision::DeniedUnknownHost),
        };

        let Some(entry) = self
            .domains
            .find_live_by_hostname(&self.pool, hostname.as_str())
            .await?
        else {
            return Ok(AuthorizeDecision::DeniedUnknownHost);
        };

        if !matches!(
            entry.status.as_str(),
            "verified" | "issuing_cert" | "live" | "degraded"
        ) {
            return Ok(AuthorizeDecision::DeniedNotVerified);
        }

        if !self.tenants.is_active(entry.tenant_id).await? {
            return Ok(AuthorizeDecision::DeniedTenantInactive);
        }

        Ok(AuthorizeDecision::Allowed)
    }
}
