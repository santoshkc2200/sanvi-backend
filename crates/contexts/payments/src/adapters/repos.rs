//! SQLx persistence for the payments context.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use sanvi_persistence::{PlatformScope, ScopedTx};

use crate::domain::connection::ProviderConnection;
use crate::domain::error::PaymentsError;
use crate::domain::ports::{
    ConnectionLifecycleStore, ProviderConnectionRepository, WebhookEventRepository,
};
use crate::domain::provider::{ConnectionStatus, ProviderKind};
use crate::domain::webhook::{WebhookEventRecord, event_created};

fn storage(err: sqlx::Error) -> PaymentsError {
    PaymentsError::Storage(err.to_string())
}

#[derive(sqlx::FromRow)]
struct ProviderConnectionRow {
    id: Uuid,
    tenant_id: Uuid,
    provider: String,
    external_account_id: String,
    status: String,
    capabilities: Value,
    requirements: Value,
    country: Option<String>,
    default_currency: Option<String>,
    connected_at: Option<DateTime<Utc>>,
    last_synced_at: Option<DateTime<Utc>>,
    disconnected_at: Option<DateTime<Utc>>,
}

impl TryFrom<ProviderConnectionRow> for ProviderConnection {
    type Error = PaymentsError;

    fn try_from(row: ProviderConnectionRow) -> Result<Self, Self::Error> {
        let provider = ProviderKind::parse(&row.provider).ok_or_else(|| {
            PaymentsError::Storage(format!("unknown provider kind {:?}", row.provider))
        })?;
        let status = ConnectionStatus::parse(&row.status)
            .ok_or_else(|| PaymentsError::Storage(format!("unknown status {:?}", row.status)))?;
        Ok(ProviderConnection {
            id: row.id,
            tenant_id: row.tenant_id,
            provider,
            external_account_id: row.external_account_id,
            status,
            capabilities: row.capabilities,
            requirements: row.requirements,
            country: row.country,
            default_currency: row.default_currency,
            connected_at: row.connected_at,
            last_synced_at: row.last_synced_at,
            disconnected_at: row.disconnected_at,
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxProviderConnectionRepository;

impl SqlxProviderConnectionRepository {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ProviderConnectionRepository for SqlxProviderConnectionRepository {
    async fn insert(
        &self,
        scope: &mut ScopedTx<'_>,
        connection: &ProviderConnection,
    ) -> Result<(), PaymentsError> {
        sqlx::query!(
            r#"INSERT INTO payments.provider_connections
               (id, tenant_id, provider, external_account_id, status,
                capabilities, requirements, country, default_currency,
                connected_at, last_synced_at, disconnected_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"#,
            connection.id,
            connection.tenant_id,
            connection.provider.as_str(),
            connection.external_account_id,
            connection.status.as_str(),
            connection.capabilities,
            connection.requirements,
            connection.country,
            connection.default_currency,
            connection.connected_at,
            connection.last_synced_at,
            connection.disconnected_at,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| {
            // The partial unique index is the backstop behind the use
            // case's guard: a concurrent different-key create loses here
            // with a 409-shaped domain error, never a raw 500.
            if let sqlx::Error::Database(db) = &err
                && db.constraint() == Some("one_active_provider_per_tenant")
            {
                return PaymentsError::ConnectionExists(
                    "an active connection already exists".to_string(),
                );
            }
            storage(err)
        })?;
        Ok(())
    }

    async fn find_active(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        provider: &str,
    ) -> Result<Option<ProviderConnection>, PaymentsError> {
        let row = sqlx::query_as!(
            ProviderConnectionRow,
            r#"SELECT id, tenant_id, provider, external_account_id, status,
                      capabilities, requirements, country, default_currency,
                      connected_at, last_synced_at, disconnected_at
               FROM payments.provider_connections
               WHERE tenant_id = $1 AND provider = $2 AND disconnected_at IS NULL"#,
            tenant_id,
            provider,
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(TryFrom::try_from).transpose()
    }

    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<ProviderConnection>, PaymentsError> {
        let rows = sqlx::query_as!(
            ProviderConnectionRow,
            r#"SELECT id, tenant_id, provider, external_account_id, status,
                      capabilities, requirements, country, default_currency,
                      connected_at, last_synced_at, disconnected_at
               FROM payments.provider_connections
               WHERE tenant_id = $1
               ORDER BY connected_at DESC NULLS LAST, id DESC"#,
            tenant_id,
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(TryFrom::try_from).collect()
    }

    async fn find_by_id(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        id: Uuid,
    ) -> Result<Option<ProviderConnection>, PaymentsError> {
        let row = sqlx::query_as!(
            ProviderConnectionRow,
            r#"SELECT id, tenant_id, provider, external_account_id, status,
                      capabilities, requirements, country, default_currency,
                      connected_at, last_synced_at, disconnected_at
               FROM payments.provider_connections
               WHERE tenant_id = $1 AND id = $2"#,
            tenant_id,
            id,
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(TryFrom::try_from).transpose()
    }

    async fn update(
        &self,
        scope: &mut ScopedTx<'_>,
        connection: &ProviderConnection,
    ) -> Result<(), PaymentsError> {
        sqlx::query!(
            r#"UPDATE payments.provider_connections
               SET external_account_id = $3, status = $4, capabilities = $5,
                   requirements = $6, country = $7, default_currency = $8,
                   connected_at = $9, last_synced_at = $10, disconnected_at = $11,
                   updated_at = now()
               WHERE id = $1 AND tenant_id = $2"#,
            connection.id,
            connection.tenant_id,
            connection.external_account_id,
            connection.status.as_str(),
            connection.capabilities,
            connection.requirements,
            connection.country,
            connection.default_currency,
            connection.connected_at,
            connection.last_synced_at,
            connection.disconnected_at,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 09.3: platform-scope lifecycle access (worker: dispatcher + reconciler).
//
// The shared RLS policy admits audited `PlatformScope` transactions, so
// cross-tenant maintenance reads and writes the same rows the tenant API
// sees — one truth, two doors.
// ---------------------------------------------------------------------------

#[async_trait]
impl ConnectionLifecycleStore for SqlxProviderConnectionRepository {
    async fn find_live_by_external_account(
        &self,
        scope: &mut PlatformScope<'_>,
        external_account_id: &str,
    ) -> Result<Option<ProviderConnection>, PaymentsError> {
        let row = sqlx::query_as!(
            ProviderConnectionRow,
            r#"SELECT id, tenant_id, provider, external_account_id, status,
                      capabilities, requirements, country, default_currency,
                      connected_at, last_synced_at, disconnected_at
               FROM payments.provider_connections
               WHERE external_account_id = $1 AND disconnected_at IS NULL"#,
            external_account_id,
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(TryFrom::try_from).transpose()
    }

    async fn list_due_for_sync(
        &self,
        scope: &mut PlatformScope<'_>,
        stale_before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<ProviderConnection>, PaymentsError> {
        let rows = sqlx::query_as!(
            ProviderConnectionRow,
            r#"SELECT id, tenant_id, provider, external_account_id, status,
                      capabilities, requirements, country, default_currency,
                      connected_at, last_synced_at, disconnected_at
               FROM payments.provider_connections
               WHERE disconnected_at IS NULL
                 AND status <> 'rejected'
                 AND (last_synced_at IS NULL OR last_synced_at < $1)
               ORDER BY last_synced_at NULLS FIRST, id
               LIMIT $2"#,
            stale_before,
            limit,
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(TryFrom::try_from).collect()
    }

    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        connection: &ProviderConnection,
    ) -> Result<(), PaymentsError> {
        sqlx::query!(
            r#"UPDATE payments.provider_connections
               SET status = $2, capabilities = $3, requirements = $4,
                   country = $5, default_currency = $6,
                   connected_at = $7, last_synced_at = $8,
                   updated_at = now()
               WHERE id = $1"#,
            connection.id,
            connection.status.as_str(),
            connection.capabilities,
            connection.requirements,
            connection.country,
            connection.default_currency,
            connection.connected_at,
            connection.last_synced_at,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn count_by_status(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<(String, i64)>, PaymentsError> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT status, count(*)::bigint FROM payments.provider_connections
             WHERE disconnected_at IS NULL GROUP BY status",
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(rows)
    }
}

// ---------------------------------------------------------------------------
// 09.3: the Connect webhook event store (`payments.webhook_events`).
//
// Deliberately NOT tenant-RLS'd: events arrive before the tenant is known
// and are routed by account id. Runtime queries (no compile-time macros)
// keep this table's access independent of the offline query cache.
// ---------------------------------------------------------------------------

/// The stored row shape for queue draining and dead-letter triage.
#[derive(sqlx::FromRow)]
struct WebhookEventRow {
    stripe_event_id: String,
    account_id: Option<String>,
    event_type: String,
    api_version: Option<String>,
    payload: Value,
    received_at: DateTime<Utc>,
    attempts: i32,
    error: Option<String>,
}

impl From<WebhookEventRow> for WebhookEventRecord {
    fn from(row: WebhookEventRow) -> Self {
        Self {
            created: event_created(&row.payload, row.received_at),
            stripe_event_id: row.stripe_event_id,
            account_id: row.account_id,
            event_type: row.event_type,
            api_version: row.api_version,
            payload: row.payload,
            received_at: row.received_at,
            attempts: row.attempts,
            last_error: row.error,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxWebhookEventRepository;

impl SqlxWebhookEventRepository {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl WebhookEventRepository for SqlxWebhookEventRepository {
    async fn insert_ignore(
        &self,
        scope: &mut PlatformScope<'_>,
        record: &WebhookEventRecord,
    ) -> Result<bool, PaymentsError> {
        let result = sqlx::query(
            "INSERT INTO payments.webhook_events
               (stripe_event_id, account_id, type, api_version, payload, received_at)
             VALUES ($1, $2, $3, $4, $5, now())
             ON CONFLICT (stripe_event_id) DO NOTHING",
        )
        .bind(&record.stripe_event_id)
        .bind(&record.account_id)
        .bind(&record.event_type)
        .bind(&record.api_version)
        .bind(&record.payload)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(result.rows_affected() > 0)
    }

    async fn fetch_unprocessed(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<WebhookEventRecord>, PaymentsError> {
        let rows: Vec<WebhookEventRow> = sqlx::query_as(
            "SELECT stripe_event_id, account_id, type AS event_type, api_version,
                    payload, received_at, attempts, error
             FROM payments.webhook_events
             WHERE processed_at IS NULL AND attempts < 5
             ORDER BY received_at, stripe_event_id LIMIT $1",
        )
        .bind(i64::from(limit))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn mark_processed(
        &self,
        scope: &mut PlatformScope<'_>,
        stripe_event_id: &str,
    ) -> Result<(), PaymentsError> {
        sqlx::query(
            "UPDATE payments.webhook_events SET processed_at = now(), error = NULL
             WHERE stripe_event_id = $1",
        )
        .bind(stripe_event_id)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn mark_attempt_failed(
        &self,
        scope: &mut PlatformScope<'_>,
        stripe_event_id: &str,
        error: &str,
    ) -> Result<(), PaymentsError> {
        sqlx::query(
            "UPDATE payments.webhook_events SET attempts = attempts + 1, error = $2
             WHERE stripe_event_id = $1",
        )
        .bind(stripe_event_id)
        .bind(error)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn unprocessed_count(&self, scope: &mut PlatformScope<'_>) -> Result<i64, PaymentsError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM payments.webhook_events
             WHERE processed_at IS NULL AND attempts < 5",
        )
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(count)
    }

    async fn dead_letter_count(&self, scope: &mut PlatformScope<'_>) -> Result<i64, PaymentsError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM payments.webhook_events
             WHERE processed_at IS NULL AND attempts >= 5",
        )
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(count)
    }

    async fn dead_letters(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<WebhookEventRecord>, PaymentsError> {
        let rows: Vec<WebhookEventRow> = sqlx::query_as(
            "SELECT stripe_event_id, account_id, type AS event_type, api_version,
                    payload, received_at, attempts, error
             FROM payments.webhook_events
             WHERE processed_at IS NULL AND attempts >= 5
             ORDER BY received_at, stripe_event_id LIMIT $1",
        )
        .bind(i64::from(limit))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn replay(
        &self,
        scope: &mut PlatformScope<'_>,
        stripe_event_id: &str,
    ) -> Result<bool, PaymentsError> {
        let result = sqlx::query(
            "UPDATE payments.webhook_events SET attempts = 0, error = NULL
             WHERE stripe_event_id = $1 AND processed_at IS NULL AND attempts >= 5",
        )
        .bind(stripe_event_id)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(result.rows_affected() > 0)
    }
}

// ---------------------------------------------------------------------------
// 09.4: the checkout aggregate and the payment read model.
//
// Runtime queries (no compile-time macros), like the webhook event store
// above, so these tables' access stays independent of the offline query
// cache. Tenant-scoped methods run under `ScopedTx` (RLS enforced);
// webhook-driven maintenance runs under the audited `PlatformScope`.
// ---------------------------------------------------------------------------

use crate::domain::checkout::{Checkout, CheckoutStatus, MerchantOrderRef};
use crate::domain::money::Money;
use crate::domain::payment::{PaymentFailure, PaymentRecord, PaymentStatus};
use crate::domain::ports::{CheckoutLifecycleStore, CheckoutRepository};

#[derive(sqlx::FromRow)]
struct CheckoutRow {
    id: Uuid,
    tenant_id: Uuid,
    connection_id: Uuid,
    reference: String,
    customer_ref: Option<Value>,
    amount_minor: i64,
    currency: String,
    status: String,
    external_session_id: Option<String>,
    idempotency_key: Option<Uuid>,
    locale: Option<String>,
    expires_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl TryFrom<CheckoutRow> for Checkout {
    type Error = PaymentsError;

    fn try_from(row: CheckoutRow) -> Result<Self, Self::Error> {
        let status = CheckoutStatus::parse(&row.status).ok_or_else(|| {
            PaymentsError::Storage(format!("unknown checkout status {:?}", row.status))
        })?;
        Ok(Self {
            id: row.id,
            tenant_id: row.tenant_id,
            connection_id: row.connection_id,
            reference: MerchantOrderRef::parse(&row.reference)?,
            customer_ref: row.customer_ref,
            amount: Money::new(row.amount_minor, &row.currency)?,
            status,
            external_session_id: row.external_session_id,
            idempotency_key: row.idempotency_key,
            locale: row.locale,
            expires_at: row.expires_at,
            completed_at: row.completed_at,
            created_at: row.created_at,
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxCheckoutRepository;

impl SqlxCheckoutRepository {
    pub const fn new() -> Self {
        Self
    }
}

fn map_checkout_err(err: sqlx::Error) -> PaymentsError {
    if let sqlx::Error::Database(db) = &err
        && db.constraint() == Some("checkouts_idempotency_idx")
    {
        // The (tenant_id, idempotency_key) partial unique index: another
        // request already owns this key. The use case adopts its row —
        // a double-clicked buy button must not fail, it must dedupe.
        return PaymentsError::IdempotencyConflict(
            "a checkout already exists for this Idempotency-Key".to_string(),
        );
    }
    storage(err)
}

fn row_to_checkout(row: CheckoutRow) -> Result<Checkout, PaymentsError> {
    TryInto::<Checkout>::try_into(row)
}

#[async_trait]
impl CheckoutRepository for SqlxCheckoutRepository {
    async fn insert(
        &self,
        scope: &mut ScopedTx<'_>,
        checkout: &Checkout,
    ) -> Result<(), PaymentsError> {
        sqlx::query(
            "INSERT INTO payments.checkouts
               (id, tenant_id, connection_id, reference, customer_ref,
                amount_minor, currency, status, external_session_id,
                idempotency_key, locale, expires_at, completed_at, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
        )
        .bind(checkout.id)
        .bind(checkout.tenant_id)
        .bind(checkout.connection_id)
        .bind(checkout.reference.as_str())
        .bind(&checkout.customer_ref)
        .bind(checkout.amount.amount_minor)
        .bind(checkout.amount.currency.as_str())
        .bind(checkout.status.as_str())
        .bind(&checkout.external_session_id)
        .bind(checkout.idempotency_key)
        .bind(&checkout.locale)
        .bind(checkout.expires_at)
        .bind(checkout.completed_at)
        .bind(checkout.created_at)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_checkout_err)?;
        Ok(())
    }

    async fn find_by_id(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        id: Uuid,
    ) -> Result<Option<Checkout>, PaymentsError> {
        let row = sqlx::query_as::<_, CheckoutRow>(
            "SELECT id, tenant_id, connection_id, reference, customer_ref,
                    amount_minor, currency, status, external_session_id,
                    idempotency_key, locale, expires_at, completed_at, created_at
             FROM payments.checkouts
             WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant_id)
        .bind(id)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(row_to_checkout).transpose()
    }

    async fn find_by_idempotency_key(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        key: Uuid,
    ) -> Result<Option<Checkout>, PaymentsError> {
        let row = sqlx::query_as::<_, CheckoutRow>(
            "SELECT id, tenant_id, connection_id, reference, customer_ref,
                    amount_minor, currency, status, external_session_id,
                    idempotency_key, locale, expires_at, completed_at, created_at
             FROM payments.checkouts
             WHERE tenant_id = $1 AND idempotency_key = $2",
        )
        .bind(tenant_id)
        .bind(key)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(row_to_checkout).transpose()
    }

    async fn update(
        &self,
        scope: &mut ScopedTx<'_>,
        checkout: &Checkout,
    ) -> Result<(), PaymentsError> {
        sqlx::query(
            "UPDATE payments.checkouts
             SET status = $3, external_session_id = $4, expires_at = $5,
                 completed_at = $6, customer_ref = $7, locale = $8,
                 updated_at = now()
             WHERE id = $1 AND tenant_id = $2",
        )
        .bind(checkout.id)
        .bind(checkout.tenant_id)
        .bind(checkout.status.as_str())
        .bind(&checkout.external_session_id)
        .bind(checkout.expires_at)
        .bind(checkout.completed_at)
        .bind(&checkout.customer_ref)
        .bind(&checkout.locale)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn latest_payment_for_checkout(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        checkout_id: Uuid,
    ) -> Result<Option<PaymentRecord>, PaymentsError> {
        let row = sqlx::query_as::<_, PaymentRow>(
            "SELECT id, tenant_id, checkout_id, external_payment_id, status,
                    amount_minor, currency, fee_amount_minor, fee_currency,
                    failure, captured_at
             FROM payments.payments
             WHERE tenant_id = $1 AND checkout_id = $2
             ORDER BY updated_at DESC, created_at DESC, id DESC LIMIT 1",
        )
        .bind(tenant_id)
        .bind(checkout_id)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(TryFrom::try_from).transpose()
    }
}

// -- platform-scope access for the webhook dispatcher -----------------------

impl TryFrom<PaymentRow> for PaymentRecord {
    type Error = PaymentsError;

    fn try_from(row: PaymentRow) -> Result<Self, Self::Error> {
        let status = PaymentStatus::parse(&row.status).ok_or_else(|| {
            PaymentsError::Storage(format!("unknown payment status {:?}", row.status))
        })?;
        let amount = Money::new(row.amount_minor, &row.currency)?;
        let fee = match (row.fee_amount_minor, row.fee_currency) {
            (Some(minor), Some(code)) => Some(Money::new(minor, &code)?),
            _ => None,
        };
        let failure = row.failure.and_then(|raw| {
            Some(PaymentFailure {
                code: raw.get("code")?.as_str()?.to_string(),
                message: raw
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        });
        Ok(Self {
            id: row.id,
            tenant_id: row.tenant_id,
            checkout_id: row.checkout_id,
            external_payment_id: row.external_payment_id,
            status,
            amount,
            fee,
            failure,
            captured_at: row.captured_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct PaymentRow {
    id: Uuid,
    tenant_id: Uuid,
    checkout_id: Option<Uuid>,
    external_payment_id: String,
    status: String,
    amount_minor: i64,
    currency: String,
    fee_amount_minor: Option<i64>,
    fee_currency: Option<String>,
    failure: Option<Value>,
    captured_at: Option<DateTime<Utc>>,
}

/// The checkout lifecycle store: one struct, both doors. Tenant paths use
/// [`SqlxCheckoutRepository`]; the dispatcher uses this impl.
#[async_trait]
impl CheckoutLifecycleStore for SqlxCheckoutRepository {
    async fn find_by_external_session(
        &self,
        scope: &mut PlatformScope<'_>,
        external_session_id: &str,
    ) -> Result<Option<Checkout>, PaymentsError> {
        let row = sqlx::query_as::<_, CheckoutRow>(
            "SELECT id, tenant_id, connection_id, reference, customer_ref,
                    amount_minor, currency, status, external_session_id,
                    idempotency_key, locale, expires_at, completed_at, created_at
             FROM payments.checkouts
             WHERE external_session_id = $1",
        )
        .bind(external_session_id)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(row_to_checkout).transpose()
    }

    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Uuid,
        checkout_id: Uuid,
    ) -> Result<Option<Checkout>, PaymentsError> {
        let row = sqlx::query_as::<_, CheckoutRow>(
            "SELECT id, tenant_id, connection_id, reference, customer_ref,
                    amount_minor, currency, status, external_session_id,
                    idempotency_key, locale, expires_at, completed_at, created_at
             FROM payments.checkouts
             WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant_id)
        .bind(checkout_id)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(row_to_checkout).transpose()
    }

    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        checkout: &Checkout,
    ) -> Result<(), PaymentsError> {
        sqlx::query(
            "UPDATE payments.checkouts
             SET status = $3, external_session_id = $4, expires_at = $5,
                 completed_at = $6, updated_at = now()
             WHERE id = $1 AND tenant_id = $2",
        )
        .bind(checkout.id)
        .bind(checkout.tenant_id)
        .bind(checkout.status.as_str())
        .bind(&checkout.external_session_id)
        .bind(checkout.expires_at)
        .bind(checkout.completed_at)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn upsert_payment(
        &self,
        scope: &mut PlatformScope<'_>,
        payment: &PaymentRecord,
    ) -> Result<(), PaymentsError> {
        let failure = payment
            .failure
            .as_ref()
            .map(|f| serde_json::json!({ "code": f.code, "message": f.message }));
        sqlx::query(
            "INSERT INTO payments.payments
               (id, tenant_id, checkout_id, external_payment_id, status,
                amount_minor, currency, fee_amount_minor, fee_currency,
                failure, captured_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (tenant_id, external_payment_id) DO UPDATE SET
               checkout_id = COALESCE(payments.payments.checkout_id, EXCLUDED.checkout_id),
               status = EXCLUDED.status,
               fee_amount_minor = EXCLUDED.fee_amount_minor,
               fee_currency = EXCLUDED.fee_currency,
               failure = EXCLUDED.failure,
               captured_at = COALESCE(EXCLUDED.captured_at, payments.payments.captured_at),
               updated_at = now()",
        )
        .bind(payment.id)
        .bind(payment.tenant_id)
        .bind(payment.checkout_id)
        .bind(&payment.external_payment_id)
        .bind(payment.status.as_str())
        .bind(payment.amount.amount_minor)
        .bind(payment.amount.currency.as_str())
        .bind(payment.fee.as_ref().map(|fee| fee.amount_minor))
        .bind(
            payment
                .fee
                .as_ref()
                .map(|fee| fee.currency.as_str().to_string()),
        )
        .bind(failure)
        .bind(payment.captured_at)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn find_payment_by_external_id(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Uuid,
        external_payment_id: &str,
    ) -> Result<Option<PaymentRecord>, PaymentsError> {
        let row = sqlx::query_as::<_, PaymentRow>(
            "SELECT id, tenant_id, checkout_id, external_payment_id, status,
                    amount_minor, currency, fee_amount_minor, fee_currency,
                    failure, captured_at
             FROM payments.payments
             WHERE tenant_id = $1 AND external_payment_id = $2",
        )
        .bind(tenant_id)
        .bind(external_payment_id)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(TryFrom::try_from).transpose()
    }
}
