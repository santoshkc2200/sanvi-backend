//! PostgreSQL credential store. Tokens are sealed with a per-tenant DEK from
//! `accounting.tenant_secret_keys`, wrapped by a KEK held only in deployed
//! configuration.

use std::{collections::HashMap, sync::Arc, sync::Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_crypto::Key;
use sanvi_kernel::{EventEnvelope, EventId, RawIdGenerator, TenantId};
use sanvi_messaging::OutboxWriter;
use sanvi_persistence::{ScopedTx, TxManager};
use serde::Serialize;

use crate::{
    application::ports::{AuditRecord, CredentialChange, CredentialStore},
    domain::{
        AccountingError, BookId, ConnectionId, Credential, CredentialStatus, OAuthStateClaims,
        Secret, Tokens,
    },
};

fn storage(error: impl std::fmt::Display) -> AccountingError {
    AccountingError::Storage(error.to_string())
}

fn crypto(_: sanvi_crypto::CryptoError) -> AccountingError {
    AccountingError::Storage("accounting credential encryption failed".into())
}

#[derive(Serialize)]
struct CredentialEvent {
    book_id: BookId,
    connection_id: ConnectionId,
    operation: &'static str,
}

struct Sealed {
    refresh_ciphertext: Option<Vec<u8>>,
    refresh_issued_at: Option<DateTime<Utc>>,
    access_ciphertext: Option<Vec<u8>>,
    access_expires_at: Option<DateTime<Utc>>,
}

pub struct PostgresCredentialStore {
    transactions: TxManager,
    ids: Arc<dyn RawIdGenerator>,
    kek: Key,
    kek_id: String,
    deks: Mutex<HashMap<TenantId, Key>>,
}

impl PostgresCredentialStore {
    pub fn new(
        transactions: TxManager,
        ids: Arc<dyn RawIdGenerator>,
        kek: Key,
        kek_id: String,
    ) -> Self {
        Self {
            transactions,
            ids,
            kek,
            kek_id,
            deks: Mutex::new(HashMap::new()),
        }
    }

    async fn begin(&self, tenant: TenantId) -> Result<ScopedTx<'_>, AccountingError> {
        self.transactions
            .begin_for(tenant.as_i64())
            .await
            .map_err(storage)
    }

    /// Concurrent first credentials for a tenant converge on one DEK.
    async fn dek(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant: TenantId,
    ) -> Result<Key, AccountingError> {
        if let Some(dek) = self
            .deks
            .lock()
            .ok()
            .and_then(|cache| cache.get(&tenant).copied())
        {
            return Ok(dek);
        }
        let candidate =
            sanvi_crypto::seal(&self.kek, &sanvi_crypto::generate_key()).map_err(crypto)?;
        let row = sqlx::query!(
            "INSERT INTO accounting.tenant_secret_keys (tenant_id, wrapped_dek, kek_id) VALUES ($1, $2, $3) ON CONFLICT (tenant_id) DO UPDATE SET tenant_id = EXCLUDED.tenant_id RETURNING wrapped_dek, kek_id",
            tenant.as_i64(), candidate, self.kek_id
        ).fetch_one(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?;
        if row.kek_id != self.kek_id {
            return Err(AccountingError::Storage(
                "tenant DEK is wrapped with an unknown KEK".into(),
            ));
        }
        let dek: Key = sanvi_crypto::open(&self.kek, &row.wrapped_dek)
            .map_err(crypto)?
            .try_into()
            .map_err(|_| AccountingError::Storage("tenant DEK must be 32 bytes".into()))?;
        // The returned key may still be uncommitted. Only publish it after
        // the credential transaction commits successfully.
        Ok(dek)
    }

    fn seal(dek: &Key, credential: &Credential) -> Result<Sealed, AccountingError> {
        Ok(match &credential.tokens {
            Some(tokens) => Sealed {
                refresh_ciphertext: Some(
                    sanvi_crypto::seal(dek, tokens.refresh_token.expose().as_bytes())
                        .map_err(crypto)?,
                ),
                refresh_issued_at: Some(tokens.refresh_issued_at),
                access_ciphertext: Some(
                    sanvi_crypto::seal(dek, tokens.access_token.expose().as_bytes())
                        .map_err(crypto)?,
                ),
                access_expires_at: Some(tokens.access_expires_at),
            },
            None => Sealed {
                refresh_ciphertext: None,
                refresh_issued_at: None,
                access_ciphertext: None,
                access_expires_at: None,
            },
        })
    }

    fn open(dek: &Key, ciphertext: &[u8]) -> Result<Secret, AccountingError> {
        String::from_utf8(sanvi_crypto::open(dek, ciphertext).map_err(crypto)?)
            .map(Secret::new)
            .map_err(|_| AccountingError::Storage("credential is not UTF-8".into()))
    }

    async fn append(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant: TenantId,
        credential: &Credential,
    ) -> Result<(), AccountingError> {
        let operation = match credential.status {
            CredentialStatus::Active => "connection_authorized",
            CredentialStatus::ReauthorizationRequired => "reauthorization_required",
            CredentialStatus::Revoked => "credential_revoked",
        };
        let event = CredentialEvent {
            book_id: credential.book_id,
            connection_id: credential.connection_id,
            operation,
        };
        let envelope = EventEnvelope::new(
            EventId::from_snowflake(self.ids.next_id().map_err(storage)?),
            "accounting.connection",
            credential.connection_id.as_snowflake(),
            format!("accounting.{operation}"),
            serde_json::to_value(&event).map_err(storage)?,
        )
        .with_metadata("tenant_id", serde_json::json!(tenant.to_string()));
        OutboxWriter
            .append(scope.tx_mut(), &envelope)
            .await
            .map_err(storage)
    }

    async fn write(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant: TenantId,
        credential: &Credential,
    ) -> Result<Key, AccountingError> {
        let dek = self.dek(scope, tenant).await?;
        let sealed = Self::seal(&dek, credential)?;
        sqlx::query!(
            "INSERT INTO accounting.provider_credentials (connection_id, tenant_id, book_id, status, refresh_ciphertext, refresh_issued_at, access_ciphertext, access_expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (connection_id) DO UPDATE SET status = EXCLUDED.status, refresh_ciphertext = EXCLUDED.refresh_ciphertext, refresh_issued_at = EXCLUDED.refresh_issued_at, access_ciphertext = EXCLUDED.access_ciphertext, access_expires_at = EXCLUDED.access_expires_at, updated_at = now() WHERE accounting.provider_credentials.tenant_id = EXCLUDED.tenant_id AND accounting.provider_credentials.book_id = EXCLUDED.book_id",
            credential.connection_id.as_i64(), tenant.as_i64(), credential.book_id.as_i64(),
            credential.status.as_str(), sealed.refresh_ciphertext, sealed.refresh_issued_at,
            sealed.access_ciphertext, sealed.access_expires_at
        ).execute(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?
            .rows_affected()
            .eq(&1)
            .then_some(dek)
            .ok_or(AccountingError::Conflict("connection belongs to another book"))
    }

    async fn lock(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant: TenantId,
        connection_id: ConnectionId,
    ) -> Result<Credential, AccountingError> {
        let row = sqlx::query!(
            "SELECT book_id, status, refresh_ciphertext, refresh_issued_at, access_ciphertext, access_expires_at FROM accounting.provider_credentials WHERE tenant_id = $1 AND connection_id = $2 FOR UPDATE",
            tenant.as_i64(), connection_id.as_i64()
        ).fetch_optional(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?.ok_or(AccountingError::NotFound)?;
        let status = CredentialStatus::parse(&row.status)
            .ok_or_else(|| AccountingError::Storage("unknown credential status".into()))?;
        let tokens = match (
            row.refresh_ciphertext,
            row.refresh_issued_at,
            row.access_ciphertext,
            row.access_expires_at,
        ) {
            (Some(refresh), Some(refresh_issued_at), Some(access), Some(access_expires_at)) => {
                let dek = self.dek(scope, tenant).await?;
                Some(Tokens {
                    refresh_token: Self::open(&dek, &refresh)?,
                    refresh_issued_at,
                    access_token: Self::open(&dek, &access)?,
                    access_expires_at,
                })
            }
            (None, None, None, None) => None,
            _ => return Err(AccountingError::Storage("partial credential row".into())),
        };
        Ok(Credential {
            book_id: BookId::try_from(row.book_id).map_err(storage)?,
            connection_id,
            status,
            tokens,
        })
    }
}

#[async_trait]
impl CredentialStore for PostgresCredentialStore {
    async fn issue_state(&self, claims: &OAuthStateClaims) -> Result<(), AccountingError> {
        let mut scope = self.begin(claims.tenant_id).await?;
        sqlx::query!(
            "INSERT INTO accounting.oauth_states (nonce, tenant_id, user_id, book_id, connection_id, provider, expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
            claims.nonce, claims.tenant_id.as_i64(), claims.user_id.as_i64(), claims.book_id.as_i64(),
            claims.connection_id.as_i64(), provider_name(claims), claims.expires_at
        ).execute(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?;
        scope.commit().await.map_err(storage)
    }

    async fn consume_state(&self, claims: &OAuthStateClaims) -> Result<bool, AccountingError> {
        let mut scope = self.begin(claims.tenant_id).await?;
        let consumed = sqlx::query!(
            "UPDATE accounting.oauth_states SET redeemed_at = now() WHERE nonce = $1 AND tenant_id = $2 AND user_id = $3 AND book_id = $4 AND connection_id = $5 AND provider = $6 AND expires_at = $7 AND redeemed_at IS NULL",
            claims.nonce, claims.tenant_id.as_i64(), claims.user_id.as_i64(), claims.book_id.as_i64(),
            claims.connection_id.as_i64(), provider_name(claims), claims.expires_at
        ).execute(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?.rows_affected() == 1;
        scope.commit().await.map_err(storage)?;
        Ok(consumed)
    }

    async fn authorize(
        &self,
        tenant_id: TenantId,
        credential: Credential,
        audit: AuditRecord,
    ) -> Result<(), AccountingError> {
        let mut scope = self.begin(tenant_id).await?;
        let dek = self.write(&mut scope, tenant_id, &credential).await?;
        self.append(&mut scope, tenant_id, &credential).await?;
        crate::adapters::audit::record(&mut scope, &*self.ids, &audit).await?;
        scope.commit().await.map_err(storage)?;
        if let Ok(mut cache) = self.deks.lock() {
            cache.insert(tenant_id, dek);
        }
        Ok(())
    }

    async fn update(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectionId,
        change: CredentialChange,
        audit: Option<AuditRecord>,
    ) -> Result<Credential, AccountingError> {
        let mut scope = self.begin(tenant_id).await?;
        let current = self.lock(&mut scope, tenant_id, connection_id).await?;
        let previous = current.clone();
        let next = change(current).await?;
        if next.book_id != previous.book_id || next.connection_id != previous.connection_id {
            return Err(AccountingError::Storage(
                "credential identity changed".into(),
            ));
        }
        if next != previous {
            self.write(&mut scope, tenant_id, &next).await?;
            if next.status != previous.status {
                self.append(&mut scope, tenant_id, &next).await?;
                if let Some(audit) = audit {
                    crate::adapters::audit::record(&mut scope, &*self.ids, &audit).await?;
                }
            }
        }
        scope.commit().await.map_err(storage)?;
        Ok(next)
    }
}

fn provider_name(claims: &OAuthStateClaims) -> &'static str {
    match claims.provider {
        crate::domain::Provider::Freee => "freee",
        crate::domain::Provider::MoneyForward => "money_forward",
        crate::domain::Provider::Xero => "xero",
    }
}
