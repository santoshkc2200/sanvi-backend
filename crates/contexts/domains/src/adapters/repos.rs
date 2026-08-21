//! SQLx repositories for the domains context.
//!
//! RLS is never set here: tenant reads/writes arrive in a `ScopedTx` (the
//! `TxManager::begin_for` guarantee), platform sweeps in a
//! `PlatformScope`; hot-path pool reads (`find_live_by_hostname`) filter on
//! the live-claim status set explicitly because unscoped pool connections
//! see no tenant rows at all.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use sanvi_persistence::{PgPool, PlatformScope, ScopedTx};

use crate::domain::{
    Certificate, CertificateStatus, CustomDomain, DnsRecord, DnsRecordType, DomainFailure,
    DomainKind, DomainOrder, DomainRole, DomainStatus, DomainsError, Hostname, Money, OrderStatus,
    RegistrantContact, VerificationChallenge,
};

fn storage(err: impl std::fmt::Display) -> DomainsError {
    DomainsError::Storage(err.to_string())
}

fn hostname(value: &str) -> Result<Hostname, DomainsError> {
    Hostname::parse(value)
        .map_err(|err| DomainsError::Storage(format!("bad hostname in storage: {err}")))
}

// ---------------------------------------------------------------------------
// custom_domains
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxCustomDomainRepository;

impl SqlxCustomDomainRepository {
    pub const fn new() -> Self {
        Self
    }
}

#[derive(Debug, sqlx::FromRow)]
#[allow(dead_code)]
struct CustomDomainRow {
    id: Uuid,
    tenant_id: Uuid,
    hostname: String,
    normalized_hostname: String,
    kind: String,
    role: String,
    status: String,
    challenge_token: Option<String>,
    challenge_type: Option<String>,
    challenge_created_at: Option<DateTime<Utc>>,
    challenge_expires_at: Option<DateTime<Utc>>,
    detected_registrar: Option<String>,
    verified_at: Option<DateTime<Utc>>,
    last_checked_at: Option<DateTime<Utc>>,
    next_check_at: Option<DateTime<Utc>>,
    verification_attempts: i32,
    failure_code: Option<String>,
    failure_detail: Option<serde_json::Value>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn domain_from_row(row: CustomDomainRow) -> Result<CustomDomain, DomainsError> {
    let hostname = hostname(&row.hostname)?;
    let challenge = match (
        row.challenge_token,
        row.challenge_type,
        row.challenge_created_at,
        row.challenge_expires_at,
    ) {
        (Some(token), Some(challenge_type), Some(created_at), Some(expires_at)) => {
            Some(VerificationChallenge {
                token,
                challenge_type,
                created_at,
                expires_at,
            })
        }
        _ => None,
    };
    let failure = match (row.failure_code, row.failure_detail) {
        (Some(code), detail) => Some(DomainFailure {
            code,
            detail: detail
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default(),
        }),
        _ => None,
    };
    Ok(CustomDomain {
        id: row.id,
        tenant_id: row.tenant_id,
        hostname,
        kind: DomainKind::parse(&row.kind).map_err(storage)?,
        role: DomainRole::parse(&row.role).map_err(storage)?,
        status: DomainStatus::parse(&row.status).map_err(storage)?,
        challenge,
        detected_registrar: row.detected_registrar,
        verified_at: row.verified_at,
        last_checked_at: row.last_checked_at,
        next_check_at: row.next_check_at,
        verification_attempts: row.verification_attempts.max(0) as u32,
        failure,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

#[async_trait::async_trait]
impl crate::domain::CustomDomainRepository for SqlxCustomDomainRepository {
    async fn insert(
        &self,
        scope: &mut ScopedTx<'_>,
        domain: &CustomDomain,
    ) -> Result<(), DomainsError> {
        sqlx::query!(
            r#"INSERT INTO domains.custom_domains
               (id, tenant_id, hostname, normalized_hostname, kind, role, status,
                challenge_token, challenge_type, challenge_created_at, challenge_expires_at,
                detected_registrar, verified_at, last_checked_at, next_check_at,
                verification_attempts, failure_code, failure_detail, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)"#,
            domain.id,
            domain.tenant_id,
            domain.hostname.as_str(),
            domain.hostname.as_str(),
            domain.kind.as_str(),
            domain.role.as_str(),
            domain.status.as_str(),
            domain.challenge.as_ref().map(|c| c.token.as_str()),
            domain.challenge.as_ref().map(|c| c.challenge_type.as_str()),
            domain.challenge.as_ref().map(|c| c.created_at),
            domain.challenge.as_ref().map(|c| c.expires_at),
            domain.detected_registrar.as_deref(),
            domain.verified_at,
            domain.last_checked_at,
            domain.next_check_at,
            domain.verification_attempts as i32,
            domain.failure.as_ref().map(|f| f.code.as_str()),
            domain
                .failure
                .as_ref()
                .map(|f| serde_json::Value::String(f.detail.clone())),
            domain.created_at,
            domain.updated_at,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn find(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        id: Uuid,
    ) -> Result<Option<CustomDomain>, DomainsError> {
        let row = sqlx::query_as!(
            CustomDomainRow,
            "SELECT id, tenant_id, hostname, normalized_hostname, kind, role, status, \
             challenge_token, challenge_type, challenge_created_at, challenge_expires_at, \
             detected_registrar, verified_at, last_checked_at, next_check_at, \
             verification_attempts, failure_code, failure_detail, created_at, updated_at \
             FROM domains.custom_domains WHERE id = $1 AND tenant_id = $2",
            id,
            tenant_id
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(domain_from_row).transpose()
    }

    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<CustomDomain>, DomainsError> {
        let rows = sqlx::query_as!(
            CustomDomainRow,
            "SELECT id, tenant_id, hostname, normalized_hostname, kind, role, status, \
             challenge_token, challenge_type, challenge_created_at, challenge_expires_at, \
             detected_registrar, verified_at, last_checked_at, next_check_at, \
             verification_attempts, failure_code, failure_detail, created_at, updated_at \
             FROM domains.custom_domains WHERE tenant_id = $1 ORDER BY created_at ASC",
            tenant_id
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(domain_from_row).collect()
    }

    async fn find_primary(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Option<CustomDomain>, DomainsError> {
        let row = sqlx::query_as!(
            CustomDomainRow,
            "SELECT id, tenant_id, hostname, normalized_hostname, kind, role, status, \
             challenge_token, challenge_type, challenge_created_at, challenge_expires_at, \
             detected_registrar, verified_at, last_checked_at, next_check_at, \
             verification_attempts, failure_code, failure_detail, created_at, updated_at \
             FROM domains.custom_domains \
             WHERE tenant_id = $1 AND role = 'primary' AND status <> 'removed'",
            tenant_id
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(domain_from_row).transpose()
    }

    async fn update(
        &self,
        scope: &mut ScopedTx<'_>,
        domain: &CustomDomain,
    ) -> Result<(), DomainsError> {
        sqlx::query!(
            r#"UPDATE domains.custom_domains SET
                 kind = $2, role = $3, status = $4,
                 challenge_token = $5, challenge_type = $6,
                 challenge_created_at = $7, challenge_expires_at = $8,
                 detected_registrar = $9, verified_at = $10, last_checked_at = $11,
                 next_check_at = $12, verification_attempts = $13,
                 failure_code = $14, failure_detail = $15, updated_at = $16
               WHERE id = $1 AND tenant_id = $17"#,
            domain.id,
            domain.kind.as_str(),
            domain.role.as_str(),
            domain.status.as_str(),
            domain.challenge.as_ref().map(|c| c.token.as_str()),
            domain.challenge.as_ref().map(|c| c.challenge_type.as_str()),
            domain.challenge.as_ref().map(|c| c.created_at),
            domain.challenge.as_ref().map(|c| c.expires_at),
            domain.detected_registrar.as_deref(),
            domain.verified_at,
            domain.last_checked_at,
            domain.next_check_at,
            domain.verification_attempts as i32,
            domain.failure.as_ref().map(|f| f.code.as_str()),
            domain
                .failure
                .as_ref()
                .map(|f| serde_json::Value::String(f.detail.clone())),
            domain.updated_at,
            domain.tenant_id,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(unique_violation)?;
        Ok(())
    }

    async fn update_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        domain: &CustomDomain,
    ) -> Result<(), DomainsError> {
        sqlx::query!(
            r#"UPDATE domains.custom_domains SET
                 kind = $2, role = $3, status = $4,
                 challenge_token = $5, challenge_type = $6,
                 challenge_created_at = $7, challenge_expires_at = $8,
                 detected_registrar = $9, verified_at = $10, last_checked_at = $11,
                 next_check_at = $12, verification_attempts = $13,
                 failure_code = $14, failure_detail = $15, updated_at = $16
               WHERE id = $1"#,
            domain.id,
            domain.kind.as_str(),
            domain.role.as_str(),
            domain.status.as_str(),
            domain.challenge.as_ref().map(|c| c.token.as_str()),
            domain.challenge.as_ref().map(|c| c.challenge_type.as_str()),
            domain.challenge.as_ref().map(|c| c.created_at),
            domain.challenge.as_ref().map(|c| c.expires_at),
            domain.detected_registrar.as_deref(),
            domain.verified_at,
            domain.last_checked_at,
            domain.next_check_at,
            domain.verification_attempts as i32,
            domain.failure.as_ref().map(|f| f.code.as_str()),
            domain
                .failure
                .as_ref()
                .map(|f| serde_json::Value::String(f.detail.clone())),
            domain.updated_at,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(unique_violation)?;
        Ok(())
    }

    async fn find_active_by_hostname_for_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        normalized_hostname: &str,
    ) -> Result<Option<CustomDomain>, DomainsError> {
        let row = sqlx::query_as!(
            CustomDomainRow,
            "SELECT id, tenant_id, hostname, normalized_hostname, kind, role, status, \
             challenge_token, challenge_type, challenge_created_at, challenge_expires_at, \
             detected_registrar, verified_at, last_checked_at, next_check_at, \
             verification_attempts, failure_code, failure_detail, created_at, updated_at \
             FROM domains.custom_domains \
             WHERE tenant_id = $1 AND normalized_hostname = $2 AND status <> 'removed'",
            tenant_id,
            normalized_hostname
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(domain_from_row).transpose()
    }

    async fn find_live_claim_by_hostname(
        &self,
        scope: &mut PlatformScope<'_>,
        normalized_hostname: &str,
    ) -> Result<Option<CustomDomain>, DomainsError> {
        let row = sqlx::query_as!(
            CustomDomainRow,
            "SELECT id, tenant_id, hostname, normalized_hostname, kind, role, status, \
             challenge_token, challenge_type, challenge_created_at, challenge_expires_at, \
             detected_registrar, verified_at, last_checked_at, next_check_at, \
             verification_attempts, failure_code, failure_detail, created_at, updated_at \
             FROM domains.custom_domains \
             WHERE normalized_hostname = $1 \
               AND status IN ('verified', 'issuing_cert', 'live', 'degraded')",
            normalized_hostname
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(domain_from_row).transpose()
    }

    async fn find_live_by_hostname(
        &self,
        pool: &PgPool,
        normalized_hostname: &str,
    ) -> Result<Option<crate::domain::LiveHostRef>, DomainsError> {
        let row = sqlx::query!(
            "SELECT normalized_hostname, tenant_id, status FROM domains.live_hostnames \
             WHERE normalized_hostname = $1",
            normalized_hostname
        )
        .fetch_optional(pool)
        .await
        .map_err(storage)?;
        Ok(row.map(|row| crate::domain::LiveHostRef {
            tenant_id: row.tenant_id,
            hostname: row.normalized_hostname,
            status: row.status,
        }))
    }

    async fn upsert_live_host(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Uuid,
        hostname: &str,
        status: &str,
    ) -> Result<(), DomainsError> {
        sqlx::query!(
            "INSERT INTO domains.live_hostnames (normalized_hostname, tenant_id, status, updated_at)
             VALUES ($1, $2, $3, now())
             ON CONFLICT (normalized_hostname)
             DO UPDATE SET tenant_id = EXCLUDED.tenant_id, status = EXCLUDED.status, updated_at = now()",
            hostname,
            tenant_id,
            status,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn delete_live_host(
        &self,
        scope: &mut PlatformScope<'_>,
        normalized_hostname: &str,
    ) -> Result<(), DomainsError> {
        sqlx::query!(
            "DELETE FROM domains.live_hostnames WHERE normalized_hostname = $1",
            normalized_hostname
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn delete_live_host_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        normalized_hostname: &str,
    ) -> Result<(), DomainsError> {
        sqlx::query!(
            "DELETE FROM domains.live_hostnames WHERE normalized_hostname = $1",
            normalized_hostname
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn due_for_verification(
        &self,
        scope: &mut PlatformScope<'_>,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<CustomDomain>, DomainsError> {
        let rows = sqlx::query_as!(
            CustomDomainRow,
            "SELECT id, tenant_id, hostname, normalized_hostname, kind, role, status, \
             challenge_token, challenge_type, challenge_created_at, challenge_expires_at, \
             detected_registrar, verified_at, last_checked_at, next_check_at, \
             verification_attempts, failure_code, failure_detail, created_at, updated_at \
             FROM domains.custom_domains \
             WHERE status IN ('pending_setup', 'verifying') AND next_check_at <= $1 \
             ORDER BY next_check_at ASC LIMIT $2",
            now,
            limit as i64
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(domain_from_row).collect()
    }

    async fn awaiting_cert(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<CustomDomain>, DomainsError> {
        // `issuing_cert` rows are crash- or failure-stranded issuance
        // attempts: the sweep retries them alongside fresh `verified` rows.
        let rows = sqlx::query_as!(
            CustomDomainRow,
            "SELECT id, tenant_id, hostname, normalized_hostname, kind, role, status, \
             challenge_token, challenge_type, challenge_created_at, challenge_expires_at, \
             detected_registrar, verified_at, last_checked_at, next_check_at, \
             verification_attempts, failure_code, failure_detail, created_at, updated_at \
             FROM domains.custom_domains WHERE status IN ('verified', 'issuing_cert') \
             ORDER BY verified_at ASC LIMIT $1",
            limit as i64
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(domain_from_row).collect()
    }

    async fn live_or_degraded(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<CustomDomain>, DomainsError> {
        let rows = sqlx::query_as!(
            CustomDomainRow,
            "SELECT id, tenant_id, hostname, normalized_hostname, kind, role, status, \
             challenge_token, challenge_type, challenge_created_at, challenge_expires_at, \
             detected_registrar, verified_at, last_checked_at, next_check_at, \
             verification_attempts, failure_code, failure_detail, created_at, updated_at \
             FROM domains.custom_domains \
             WHERE status IN ('live', 'degraded') ORDER BY updated_at ASC LIMIT $1",
            limit as i64
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(domain_from_row).collect()
    }

    async fn platform_search(
        &self,
        scope: &mut PlatformScope<'_>,
        query: &str,
        limit: u32,
    ) -> Result<Vec<CustomDomain>, DomainsError> {
        let pattern = format!("%{query}%");
        let rows = sqlx::query_as!(
            CustomDomainRow,
            "SELECT id, tenant_id, hostname, normalized_hostname, kind, role, status, \
             challenge_token, challenge_type, challenge_created_at, challenge_expires_at, \
             detected_registrar, verified_at, last_checked_at, next_check_at, \
             verification_attempts, failure_code, failure_detail, created_at, updated_at \
             FROM domains.custom_domains \
             WHERE normalized_hostname LIKE $1 OR hostname ILIKE $2 \
             ORDER BY updated_at DESC LIMIT $3",
            pattern,
            pattern,
            limit as i64
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(domain_from_row).collect()
    }

    async fn platform_find(
        &self,
        scope: &mut PlatformScope<'_>,
        id: Uuid,
    ) -> Result<Option<CustomDomain>, DomainsError> {
        let row = sqlx::query_as!(
            CustomDomainRow,
            "SELECT id, tenant_id, hostname, normalized_hostname, kind, role, status, \
             challenge_token, challenge_type, challenge_created_at, challenge_expires_at, \
             detected_registrar, verified_at, last_checked_at, next_check_at, \
             verification_attempts, failure_code, failure_detail, created_at, updated_at \
             FROM domains.custom_domains WHERE id = $1",
            id
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(domain_from_row).transpose()
    }
}

/// Map a unique violation on the live-claim index to the domain error the
/// verification sweep understands.
fn unique_violation(err: sqlx::Error) -> DomainsError {
    if let sqlx::Error::Database(db) = &err
        && db.is_unique_violation()
    {
        return DomainsError::HostnameAlreadyClaimed("unique violation".to_string());
    }
    storage(err)
}

// ---------------------------------------------------------------------------
// orders
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxDomainOrderRepository;

impl SqlxDomainOrderRepository {
    pub const fn new() -> Self {
        Self
    }
}

#[derive(Debug, sqlx::FromRow)]
#[allow(dead_code)]
struct OrderRow {
    id: Uuid,
    tenant_id: Uuid,
    hostname: String,
    normalized_hostname: String,
    term_years: i32,
    price_minor: i64,
    currency: String,
    status: String,
    auto_renew: bool,
    whois_privacy: bool,
    registrant_contact: serde_json::Value,
    ordered_by: Option<Uuid>,
    idempotency_key: Option<Uuid>,
    external_order_ref: Option<String>,
    charge_id: Option<String>,
    registered_at: Option<DateTime<Utc>>,
    expires_at: Option<DateTime<Utc>>,
    renewal_attempts: i32,
    next_renewal_at: Option<DateTime<Utc>>,
    failure_detail: Option<serde_json::Value>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn order_from_row(row: OrderRow) -> Result<DomainOrder, DomainsError> {
    let hostname = hostname(&row.hostname)?;
    let contact: RegistrantContact =
        serde_json::from_value(row.registrant_contact).map_err(storage)?;
    Ok(DomainOrder {
        id: row.id,
        tenant_id: row.tenant_id,
        hostname,
        term_years: row.term_years.max(0) as u8,
        price: Money::new(row.price_minor, row.currency),
        status: OrderStatus::parse(&row.status).map_err(storage)?,
        auto_renew: row.auto_renew,
        whois_privacy: row.whois_privacy,
        registrant_contact: contact,
        ordered_by: row.ordered_by,
        idempotency_key: row.idempotency_key,
        external_order_ref: row.external_order_ref,
        charge_id: row.charge_id,
        registered_at: row.registered_at,
        expires_at: row.expires_at,
        renewal_attempts: row.renewal_attempts.max(0) as u32,
        next_renewal_at: row.next_renewal_at,
        failure_detail: row.failure_detail,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

#[async_trait::async_trait]
impl crate::domain::DomainOrderRepository for SqlxDomainOrderRepository {
    async fn insert(
        &self,
        scope: &mut ScopedTx<'_>,
        order: &DomainOrder,
    ) -> Result<(), DomainsError> {
        sqlx::query!(
            r#"INSERT INTO domains.orders
               (id, tenant_id, hostname, normalized_hostname, term_years, price_minor,
                currency, status, auto_renew, whois_privacy, registrant_contact, ordered_by,
                idempotency_key, external_order_ref, charge_id, registered_at, expires_at,
                renewal_attempts, next_renewal_at, failure_detail, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22)"#,
            order.id,
            order.tenant_id,
            order.hostname.as_str(),
            order.hostname.as_str(),
            order.term_years as i32,
            order.price.minor,
            order.price.currency,
            order.status.as_str(),
            order.auto_renew,
            order.whois_privacy,
            serde_json::to_value(&order.registrant_contact).map_err(storage)?,
            order.ordered_by,
            order.idempotency_key,
            order.external_order_ref.as_deref(),
            order.charge_id.as_deref(),
            order.registered_at,
            order.expires_at,
            order.renewal_attempts as i32,
            order.next_renewal_at,
            order.failure_detail,
            order.created_at,
            order.updated_at,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn find(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        id: Uuid,
    ) -> Result<Option<DomainOrder>, DomainsError> {
        let row = sqlx::query_as!(
            OrderRow,
            "SELECT id, tenant_id, hostname, normalized_hostname, term_years, price_minor, \
             currency, status, auto_renew, whois_privacy, registrant_contact, ordered_by, \
             idempotency_key, external_order_ref, charge_id, registered_at, expires_at, \
             renewal_attempts, next_renewal_at, failure_detail, created_at, updated_at \
             FROM domains.orders WHERE id = $1 AND tenant_id = $2",
            id,
            tenant_id
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(order_from_row).transpose()
    }

    async fn find_by_key(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        idempotency_key: Uuid,
    ) -> Result<Option<DomainOrder>, DomainsError> {
        let row = sqlx::query_as!(
            OrderRow,
            "SELECT id, tenant_id, hostname, normalized_hostname, term_years, price_minor, \
             currency, status, auto_renew, whois_privacy, registrant_contact, ordered_by, \
             idempotency_key, external_order_ref, charge_id, registered_at, expires_at, \
             renewal_attempts, next_renewal_at, failure_detail, created_at, updated_at \
             FROM domains.orders WHERE tenant_id = $1 AND idempotency_key = $2",
            tenant_id,
            idempotency_key
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(order_from_row).transpose()
    }

    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<DomainOrder>, DomainsError> {
        let rows = sqlx::query_as!(
            OrderRow,
            "SELECT id, tenant_id, hostname, normalized_hostname, term_years, price_minor, \
             currency, status, auto_renew, whois_privacy, registrant_contact, ordered_by, \
             idempotency_key, external_order_ref, charge_id, registered_at, expires_at, \
             renewal_attempts, next_renewal_at, failure_detail, created_at, updated_at \
             FROM domains.orders WHERE tenant_id = $1 ORDER BY created_at DESC",
            tenant_id
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(order_from_row).collect()
    }

    async fn update(
        &self,
        scope: &mut ScopedTx<'_>,
        order: &DomainOrder,
    ) -> Result<(), DomainsError> {
        sqlx::query!(
            r#"UPDATE domains.orders SET
                 term_years = $2, price_minor = $3, currency = $4, status = $5,
                 auto_renew = $6, whois_privacy = $7, registrant_contact = $8,
                 ordered_by = $9, idempotency_key = $10, external_order_ref = $11,
                 charge_id = $12, registered_at = $13, expires_at = $14,
                 renewal_attempts = $15, next_renewal_at = $16, failure_detail = $17,
                 updated_at = $18
               WHERE id = $1 AND tenant_id = $19"#,
            order.id,
            order.term_years as i32,
            order.price.minor,
            order.price.currency,
            order.status.as_str(),
            order.auto_renew,
            order.whois_privacy,
            serde_json::to_value(&order.registrant_contact).map_err(storage)?,
            order.ordered_by,
            order.idempotency_key,
            order.external_order_ref.as_deref(),
            order.charge_id.as_deref(),
            order.registered_at,
            order.expires_at,
            order.renewal_attempts as i32,
            order.next_renewal_at,
            order.failure_detail,
            order.updated_at,
            order.tenant_id,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn update_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        order: &DomainOrder,
    ) -> Result<(), DomainsError> {
        sqlx::query!(
            r#"UPDATE domains.orders SET
                 term_years = $2, price_minor = $3, currency = $4, status = $5,
                 auto_renew = $6, whois_privacy = $7, registrant_contact = $8,
                 ordered_by = $9, idempotency_key = $10, external_order_ref = $11,
                 charge_id = $12, registered_at = $13, expires_at = $14,
                 renewal_attempts = $15, next_renewal_at = $16, failure_detail = $17,
                 updated_at = $18
               WHERE id = $1"#,
            order.id,
            order.term_years as i32,
            order.price.minor,
            order.price.currency,
            order.status.as_str(),
            order.auto_renew,
            order.whois_privacy,
            serde_json::to_value(&order.registrant_contact).map_err(storage)?,
            order.ordered_by,
            order.idempotency_key,
            order.external_order_ref.as_deref(),
            order.charge_id.as_deref(),
            order.registered_at,
            order.expires_at,
            order.renewal_attempts as i32,
            order.next_renewal_at,
            order.failure_detail,
            order.updated_at,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn due_for_advancement(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<DomainOrder>, DomainsError> {
        let rows = sqlx::query_as!(
            OrderRow,
            "SELECT id, tenant_id, hostname, normalized_hostname, term_years, price_minor, \
             currency, status, auto_renew, whois_privacy, registrant_contact, ordered_by, \
             idempotency_key, external_order_ref, charge_id, registered_at, expires_at, \
             renewal_attempts, next_renewal_at, failure_detail, created_at, updated_at \
             FROM domains.orders \
             WHERE (status = 'charged' AND external_order_ref IS NULL) \
                OR status IN ('registered', 'configuring') \
                OR (status = 'failed' AND failure_detail->>'refund_error' IS NOT NULL) \
             ORDER BY updated_at ASC LIMIT $1",
            limit as i64
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(order_from_row).collect()
    }

    async fn due_for_refund_retry(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<DomainOrder>, DomainsError> {
        let rows = sqlx::query_as!(
            OrderRow,
            "SELECT id, tenant_id, hostname, normalized_hostname, term_years, price_minor, \
             currency, status, auto_renew, whois_privacy, registrant_contact, ordered_by, \
             idempotency_key, external_order_ref, charge_id, registered_at, expires_at, \
             renewal_attempts, next_renewal_at, failure_detail, created_at, updated_at \
             FROM domains.orders \
             WHERE status = 'failed' AND failure_detail->>'refund_error' IS NOT NULL \
             ORDER BY updated_at ASC LIMIT $1",
            limit as i64
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(order_from_row).collect()
    }

    async fn due_for_renewal(
        &self,
        scope: &mut PlatformScope<'_>,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<DomainOrder>, DomainsError> {
        let rows = sqlx::query_as!(
            OrderRow,
            "SELECT id, tenant_id, hostname, normalized_hostname, term_years, price_minor, \
             currency, status, auto_renew, whois_privacy, registrant_contact, ordered_by, \
             idempotency_key, external_order_ref, charge_id, registered_at, expires_at, \
             renewal_attempts, next_renewal_at, failure_detail, created_at, updated_at \
             FROM domains.orders \
             WHERE status = 'active' AND auto_renew AND expires_at IS NOT NULL \
               AND (next_renewal_at IS NULL OR next_renewal_at <= $1) \
             ORDER BY expires_at ASC LIMIT $2",
            now,
            limit as i64
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(order_from_row).collect()
    }
}

// ---------------------------------------------------------------------------
// certificates
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxCertificateRepository;

impl SqlxCertificateRepository {
    pub const fn new() -> Self {
        Self
    }
}

#[derive(Debug, sqlx::FromRow)]
struct CertificateRow {
    id: Uuid,
    domain_id: Uuid,
    tenant_id: Uuid,
    hostname: String,
    issuer: String,
    serial: String,
    not_before: DateTime<Utc>,
    not_after: DateTime<Utc>,
    renewed_at: Option<DateTime<Utc>>,
    expiry_warned_days: Option<i32>,
    status: String,
    last_error: Option<String>,
}

fn certificate_from_row(row: CertificateRow) -> Result<Certificate, DomainsError> {
    Ok(Certificate {
        id: row.id,
        domain_id: row.domain_id,
        tenant_id: row.tenant_id,
        hostname: hostname(&row.hostname)?,
        issuer: row.issuer,
        serial: row.serial,
        not_before: row.not_before,
        not_after: row.not_after,
        renewed_at: row.renewed_at,
        expiry_warned_days: row.expiry_warned_days.map(|days| days.max(0) as u32),
        status: CertificateStatus::parse(&row.status).map_err(storage)?,
        last_error: row.last_error,
    })
}

#[async_trait::async_trait]
impl crate::domain::CertificateRepository for SqlxCertificateRepository {
    async fn insert(
        &self,
        scope: &mut ScopedTx<'_>,
        cert: &Certificate,
    ) -> Result<(), DomainsError> {
        sqlx::query!(
            r#"INSERT INTO domains.certificates
               (id, domain_id, tenant_id, hostname, issuer, serial, not_before, not_after,
                renewed_at, expiry_warned_days, status, last_error)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"#,
            cert.id,
            cert.domain_id,
            cert.tenant_id,
            cert.hostname.as_str(),
            cert.issuer,
            cert.serial,
            cert.not_before,
            cert.not_after,
            cert.renewed_at,
            cert.expiry_warned_days.map(|days| days as i32),
            cert.status.as_str(),
            cert.last_error,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn insert_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        cert: &Certificate,
    ) -> Result<(), DomainsError> {
        sqlx::query!(
            r#"INSERT INTO domains.certificates
               (id, domain_id, tenant_id, hostname, issuer, serial, not_before, not_after,
                renewed_at, expiry_warned_days, status, last_error)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"#,
            cert.id,
            cert.domain_id,
            cert.tenant_id,
            cert.hostname.as_str(),
            cert.issuer,
            cert.serial,
            cert.not_before,
            cert.not_after,
            cert.renewed_at,
            cert.expiry_warned_days.map(|days| days as i32),
            cert.status.as_str(),
            cert.last_error,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn update(
        &self,
        scope: &mut ScopedTx<'_>,
        cert: &Certificate,
    ) -> Result<(), DomainsError> {
        sqlx::query!(
            r#"UPDATE domains.certificates SET
                 issuer = $2, serial = $3, not_before = $4, not_after = $5,
                 renewed_at = $6, expiry_warned_days = $7, status = $8, last_error = $9
               WHERE id = $1 AND tenant_id = $10"#,
            cert.id,
            cert.issuer,
            cert.serial,
            cert.not_before,
            cert.not_after,
            cert.renewed_at,
            cert.expiry_warned_days.map(|days| days as i32),
            cert.status.as_str(),
            cert.last_error,
            cert.tenant_id,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn update_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        cert: &Certificate,
    ) -> Result<(), DomainsError> {
        sqlx::query!(
            r#"UPDATE domains.certificates SET
                 issuer = $2, serial = $3, not_before = $4, not_after = $5,
                 renewed_at = $6, expiry_warned_days = $7, status = $8, last_error = $9
               WHERE id = $1"#,
            cert.id,
            cert.issuer,
            cert.serial,
            cert.not_before,
            cert.not_after,
            cert.renewed_at,
            cert.expiry_warned_days.map(|days| days as i32),
            cert.status.as_str(),
            cert.last_error,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn find_for_domain(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        domain_id: Uuid,
    ) -> Result<Option<Certificate>, DomainsError> {
        let row = sqlx::query_as!(
            CertificateRow,
            "SELECT id, domain_id, tenant_id, hostname, issuer, serial, not_before, not_after, \
             renewed_at, expiry_warned_days, status, last_error \
             FROM domains.certificates WHERE domain_id = $1 AND tenant_id = $2 \
             ORDER BY not_after DESC LIMIT 1",
            domain_id,
            tenant_id
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(certificate_from_row).transpose()
    }

    async fn find_for_domain_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        domain_id: Uuid,
    ) -> Result<Option<Certificate>, DomainsError> {
        let row = sqlx::query_as!(
            CertificateRow,
            "SELECT id, domain_id, tenant_id, hostname, issuer, serial, not_before, not_after, \
             renewed_at, expiry_warned_days, status, last_error \
             FROM domains.certificates WHERE domain_id = $1 \
             ORDER BY not_after DESC LIMIT 1",
            domain_id
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(certificate_from_row).transpose()
    }

    async fn due_for_renewal(
        &self,
        scope: &mut PlatformScope<'_>,
        now: DateTime<Utc>,
        renew_before_days: u32,
        limit: u32,
    ) -> Result<Vec<Certificate>, DomainsError> {
        let threshold = now + chrono::Duration::days(renew_before_days as i64);
        let rows = sqlx::query_as!(
            CertificateRow,
            "SELECT id, domain_id, tenant_id, hostname, issuer, serial, not_before, not_after, \
             renewed_at, expiry_warned_days, status, last_error \
             FROM domains.certificates \
             WHERE status = 'active' AND not_after <= $1 \
             ORDER BY not_after ASC LIMIT $2",
            threshold,
            limit as i64
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(certificate_from_row).collect()
    }

    async fn active(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<Certificate>, DomainsError> {
        let rows = sqlx::query_as!(
            CertificateRow,
            "SELECT id, domain_id, tenant_id, hostname, issuer, serial, not_before, not_after, \
             renewed_at, expiry_warned_days, status, last_error \
             FROM domains.certificates WHERE status = 'active' \
             ORDER BY not_after ASC LIMIT $1",
            limit as i64
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(certificate_from_row).collect()
    }
}

// ---------------------------------------------------------------------------
// dns_records
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxDnsRecordRepository;

impl SqlxDnsRecordRepository {
    pub const fn new() -> Self {
        Self
    }
}

#[derive(Debug, sqlx::FromRow)]
#[allow(dead_code)]
struct DnsRecordRow {
    id: Uuid,
    domain_id: Option<Uuid>,
    tenant_id: Uuid,
    record_type: String,
    name: String,
    value: String,
    ttl: i32,
    managed: bool,
}

fn record_from_row(row: DnsRecordRow) -> Result<DnsRecord, DomainsError> {
    Ok(DnsRecord {
        name: row.name,
        record_type: DnsRecordType::parse(&row.record_type)
            .ok_or_else(|| storage(format!("bad record type in storage: {}", row.record_type)))?,
        value: row.value,
        ttl: row.ttl.max(0) as u32,
    })
}

#[async_trait::async_trait]
impl crate::domain::DnsRecordRepository for SqlxDnsRecordRepository {
    async fn upsert(
        &self,
        scope: &mut ScopedTx<'_>,
        domain_id: Uuid,
        records: &[DnsRecord],
    ) -> Result<(), DomainsError> {
        for record in records {
            sqlx::query!(
                r#"INSERT INTO domains.dns_records (id, domain_id, tenant_id, record_type, name, value, ttl, managed)
                   VALUES ($1, $2, (SELECT tenant_id FROM domains.custom_domains WHERE id = $2), $3, $4, $5, $6, true)
                   ON CONFLICT (domain_id, record_type, name)
                   DO UPDATE SET value = EXCLUDED.value, ttl = EXCLUDED.ttl"#,
                Uuid::now_v7(),
                domain_id,
                record.record_type.as_str(),
                record.name,
                record.value,
                record.ttl as i32,
            )
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(storage)?;
        }
        Ok(())
    }

    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        _tenant_id: Uuid,
        domain_id: Uuid,
    ) -> Result<Vec<DnsRecord>, DomainsError> {
        let rows = sqlx::query_as!(
            DnsRecordRow,
            "SELECT id, domain_id, tenant_id, record_type, name, value, ttl, managed \
             FROM domains.dns_records WHERE domain_id = $1",
            domain_id
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(record_from_row).collect()
    }

    async fn list_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        domain_id: Uuid,
    ) -> Result<Vec<DnsRecord>, DomainsError> {
        let rows = sqlx::query_as!(
            DnsRecordRow,
            "SELECT id, domain_id, tenant_id, record_type, name, value, ttl, managed \
             FROM domains.dns_records WHERE domain_id = $1",
            domain_id
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(record_from_row).collect()
    }

    async fn remove_zone(
        &self,
        scope: &mut ScopedTx<'_>,
        domain_id: Uuid,
    ) -> Result<(), DomainsError> {
        sqlx::query!(
            "DELETE FROM domains.dns_records WHERE domain_id = $1",
            domain_id
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn remove_zone_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        domain_id: Uuid,
    ) -> Result<(), DomainsError> {
        sqlx::query!(
            "DELETE FROM domains.dns_records WHERE domain_id = $1",
            domain_id
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }
}

// The DnsProviderPort adapter over this repository lives in dns_provider.rs;
// re-exported here for the composition root.
pub use crate::adapters::dns_provider::SqlxDnsProvider;
