//! Durable, retry-safe reservations around external service calls.
use crate::{ApiError, quota_middleware::QuotaRuntime, quotas::QuotaDimension};
use chrono::{Datelike, NaiveDate, Utc};
use sanvi_kernel::TenantId;
use sha2::{Digest, Sha256};

fn storage_error(error: sqlx::Error) -> ApiError {
    let detail = error.to_string();
    if detail.contains("quota_exceeded|") {
        ApiError::from_storage_detail(&detail)
    } else {
        tracing::error!(%error,"external quota accounting failed");
        ApiError::service_unavailable("quota accounting temporarily unavailable")
    }
}
fn key_hash(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

#[derive(Debug)]
pub struct ExternalReservation {
    tenant: TenantId,
    dimension: QuotaDimension,
    key_hash: String,
    sequence: i64,
}

pub enum ExternalOutcome {
    Accepted { asset_id: Option<String> },
    Rejected,
    Ambiguous,
}

const MAX_ACTIVE_ATTEMPTS: usize = 128;

impl QuotaRuntime {
    /// Hash the canonical outbound payload, never persist its PII. Each
    /// dispatch has a durable sequence so repeated settlement is harmless.
    pub async fn reserve_external_payload(
        &self,
        tenant: TenantId,
        dimension: QuotaDimension,
        key: &str,
        amount: i64,
        payload: &[u8],
    ) -> Result<Option<ExternalReservation>, ApiError> {
        match self
            .reserve_external_inner(tenant, dimension, key, amount, Some(payload))
            .await
        {
            Err(error @ ApiError::ServiceUnavailable { .. }) if !self.enabled => {
                tracing::warn!(%error,"external quota accounting unavailable in shadow mode");
                Ok(None)
            }
            result => result,
        }
    }

    pub async fn finish_external(
        &self,
        ticket: &ExternalReservation,
        outcome: ExternalOutcome,
    ) -> Result<(), ApiError> {
        self.accounting_result(self.finish_external_inner(ticket, outcome).await)
    }

    async fn finish_external_inner(
        &self,
        ticket: &ExternalReservation,
        outcome: ExternalOutcome,
    ) -> Result<(), ApiError> {
        let manager = self.service.tx_manager();
        let mut tx = manager
            .begin_for(ticket.tenant.as_i64())
            .await
            .map_err(|_| {
                ApiError::service_unavailable("quota accounting temporarily unavailable")
            })?;
        let row=sqlx::query!("SELECT amount,period_start,accepted,ambiguous,released,active_attempts,asset_id FROM platform.external_quota_reservations WHERE tenant_id=$1 AND dimension=$2 AND key_hash=$3 FOR UPDATE",
            ticket.tenant.as_i64(), ticket.dimension.as_str(), &ticket.key_hash)
            .fetch_optional(&mut **tx.tx_mut().as_inner()).await.map_err(storage_error)?;
        if let Some(row) = row {
            let mut active = row.active_attempts;
            if active.contains(&ticket.sequence) {
                active.retain(|sequence| *sequence != ticket.sequence);
                let accepted = row.accepted || matches!(&outcome, ExternalOutcome::Accepted { .. });
                let ambiguous = row.ambiguous || matches!(&outcome, ExternalOutcome::Ambiguous);
                let released = row.released;
                let old_asset = row.asset_id;
                let asset = match &outcome {
                    ExternalOutcome::Accepted { asset_id } => asset_id.as_ref(),
                    _ => None,
                };
                if let (Some(old), Some(new)) = (&old_asset, asset)
                    && old != new
                {
                    return Err(ApiError::Conflict {
                        detail: "idempotency key returned a different upstream asset".into(),
                    });
                }
                // Every known attempt has finished rejecting the identical
                // payload. Neither an accepted nor an unknown attempt can be
                // refunded, even if a later retry receives an opaque 4xx.
                let refund = matches!(&outcome, ExternalOutcome::Rejected)
                    && active.is_empty()
                    && !accepted
                    && !ambiguous
                    && !released;
                sqlx::query!("UPDATE platform.external_quota_reservations SET active_attempts=$4,accepted=$5,ambiguous=$6,released=$7,asset_id=COALESCE(asset_id,$8) WHERE tenant_id=$1 AND dimension=$2 AND key_hash=$3",
                    ticket.tenant.as_i64(), ticket.dimension.as_str(), &ticket.key_hash,
                    &active[..], accepted, ambiguous, released||refund, asset.map(String::as_str))
                    .execute(&mut **tx.tx_mut().as_inner()).await.map_err(storage_error)?;
                if refund {
                    sqlx::query!(
                        "SELECT platform.record_quota_counter($1,$2,$3,$4)::text",
                        ticket.tenant.as_i64(),
                        ticket.dimension.as_str(),
                        row.period_start,
                        -row.amount
                    )
                    .fetch_one(&mut **tx.tx_mut().as_inner())
                    .await
                    .map_err(storage_error)?;
                }
            }
        }
        tx.commit().await.map_err(|_| {
            ApiError::service_unavailable("quota accounting temporarily unavailable")
        })?;
        Ok(())
    }
    fn accounting_result(&self, result: Result<(), ApiError>) -> Result<(), ApiError> {
        match result {
            Err(error @ ApiError::ServiceUnavailable { .. }) if !self.enabled => {
                tracing::warn!(%error, "external quota accounting unavailable in shadow mode");
                Ok(())
            }
            other => other,
        }
    }

    pub async fn reserve_external(
        &self,
        tenant: TenantId,
        dimension: QuotaDimension,
        key: &str,
        amount: i64,
    ) -> Result<(), ApiError> {
        self.accounting_result(
            self.reserve_external_inner(tenant, dimension, key, amount, None)
                .await
                .map(|_| ()),
        )
    }

    async fn reserve_external_inner(
        &self,
        tenant: TenantId,
        dimension: QuotaDimension,
        key: &str,
        amount: i64,
        payload: Option<&[u8]>,
    ) -> Result<Option<ExternalReservation>, ApiError> {
        if !matches!(
            dimension,
            QuotaDimension::StorageBytes | QuotaDimension::NotificationSends
        ) || amount <= 0
        {
            return Err(ApiError::bad_request("invalid external quota reservation"));
        }
        self.service
            .configure_enforcement(tenant, self.enabled)
            .await
            .map_err(ApiError::from)?;
        let now = Utc::now();
        let period = if dimension == QuotaDimension::StorageBytes {
            NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()
        } else {
            NaiveDate::from_ymd_opt(now.year(), now.month(), 1).unwrap()
        };
        let manager = self.service.tx_manager();
        let mut tx = manager.begin_for(tenant.as_i64()).await.map_err(|_| {
            ApiError::service_unavailable("quota accounting temporarily unavailable")
        })?;
        let inserted=sqlx::query_scalar!("INSERT INTO platform.external_quota_reservations (tenant_id,dimension,key_hash,amount,period_start)
            VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING RETURNING amount",
            tenant.as_i64(), dimension.as_str(), key_hash(key), amount, period)
            .fetch_optional(&mut **tx.tx_mut().as_inner()).await.map_err(storage_error)?;
        if inserted.is_some() {
            sqlx::query!(
                "SELECT platform.record_quota_counter($1,$2,$3,$4)::text",
                tenant.as_i64(),
                dimension.as_str(),
                period,
                amount
            )
            .fetch_one(&mut **tx.tx_mut().as_inner())
            .await
            .map_err(storage_error)?;
        }
        let row=sqlx::query!("SELECT amount,released,payload_hash,next_attempt,active_attempts FROM platform.external_quota_reservations WHERE tenant_id=$1 AND dimension=$2 AND key_hash=$3 FOR UPDATE",
            tenant.as_i64(), dimension.as_str(), key_hash(key))
            .fetch_one(&mut **tx.tx_mut().as_inner()).await.map_err(storage_error)?;
        if row.released {
            return Err(ApiError::Conflict {
                detail: "idempotency key retired after release; use a new key".into(),
            });
        }
        if row.amount != amount {
            return Err(ApiError::bad_request(
                "idempotency key reused with a different quota amount",
            ));
        }
        let ticket = if let Some(payload) = payload {
            let fingerprint = hex::encode(Sha256::digest(payload));
            let old = row.payload_hash;
            if old.as_ref().is_some_and(|old| old != &fingerprint) {
                return Err(ApiError::bad_request(
                    "idempotency key reused with a different payload",
                ));
            }
            let mut active = row.active_attempts;
            if active.len() >= MAX_ACTIVE_ATTEMPTS {
                if !self.enabled {
                    // Shadow mode must still dispatch. Preserve the allocation
                    // as ambiguous before allowing an untracked extra attempt.
                    sqlx::query!("UPDATE platform.external_quota_reservations SET ambiguous=true WHERE tenant_id=$1 AND dimension=$2 AND key_hash=$3",
                        tenant.as_i64(), dimension.as_str(), key_hash(key))
                        .execute(&mut **tx.tx_mut().as_inner()).await.map_err(storage_error)?;
                    tx.commit().await.map_err(|_| {
                        ApiError::service_unavailable("quota accounting temporarily unavailable")
                    })?;
                    return Ok(None);
                }
                return Err(ApiError::service_unavailable(
                    "too many unresolved attempts for this idempotency key; retry later",
                ));
            }
            let sequence = row.next_attempt.checked_add(1).ok_or_else(|| {
                ApiError::service_unavailable("external quota attempt sequence exhausted")
            })?;
            active.push(sequence);
            sqlx::query!("UPDATE platform.external_quota_reservations SET payload_hash=$4,next_attempt=$5,active_attempts=$6 WHERE tenant_id=$1 AND dimension=$2 AND key_hash=$3",
                tenant.as_i64(), dimension.as_str(), key_hash(key), fingerprint, sequence, &active[..])
                .execute(&mut **tx.tx_mut().as_inner()).await.map_err(storage_error)?;
            Some(ExternalReservation {
                tenant,
                dimension,
                key_hash: key_hash(key),
                sequence,
            })
        } else {
            None
        };
        tx.commit().await.map_err(|_| {
            ApiError::service_unavailable("quota accounting temporarily unavailable")
        })?;
        Ok(ticket)
    }

    pub async fn bind_asset(
        &self,
        tenant: TenantId,
        key: &str,
        asset: &str,
    ) -> Result<(), ApiError> {
        self.accounting_result(self.bind_asset_inner(tenant, key, asset).await)
    }

    async fn bind_asset_inner(
        &self,
        tenant: TenantId,
        key: &str,
        asset: &str,
    ) -> Result<(), ApiError> {
        let manager = self.service.tx_manager();
        let mut tx = manager.begin_for(tenant.as_i64()).await.map_err(|_| {
            ApiError::service_unavailable("quota accounting temporarily unavailable")
        })?;
        sqlx::query!("UPDATE platform.external_quota_reservations SET asset_id=$3,accepted=true WHERE tenant_id=$1 AND dimension='storage_bytes' AND key_hash=$2 AND NOT released",
            tenant.as_i64(), key_hash(key), asset)
            .execute(&mut **tx.tx_mut().as_inner()).await.map_err(storage_error)?;
        tx.commit().await.map_err(|_| {
            ApiError::service_unavailable("quota accounting temporarily unavailable")
        })?;
        Ok(())
    }

    /// Call only after an acknowledged upstream deletion. A repeated delete
    /// releases once, including a retry after an ambiguous local commit.
    pub async fn release_asset(&self, tenant: TenantId, asset: &str) -> Result<(), ApiError> {
        self.accounting_result(self.release_asset_inner(tenant, asset).await)
    }

    async fn release_asset_inner(&self, tenant: TenantId, asset: &str) -> Result<(), ApiError> {
        let manager = self.service.tx_manager();
        let mut tx = manager.begin_for(tenant.as_i64()).await.map_err(|_| {
            ApiError::service_unavailable("quota accounting temporarily unavailable")
        })?;
        let rows=sqlx::query!("UPDATE platform.external_quota_reservations SET released=true
            WHERE tenant_id=$1 AND dimension='storage_bytes' AND asset_id=$2 AND NOT released RETURNING amount,period_start",
            tenant.as_i64(), asset).fetch_all(&mut **tx.tx_mut().as_inner()).await.map_err(storage_error)?;
        for row in rows {
            sqlx::query!(
                "SELECT platform.record_quota_counter($1,'storage_bytes',$2,$3)::text",
                tenant.as_i64(),
                row.period_start,
                -row.amount
            )
            .fetch_one(&mut **tx.tx_mut().as_inner())
            .await
            .map_err(storage_error)?;
        }
        tx.commit().await.map_err(|_| {
            ApiError::service_unavailable("quota accounting temporarily unavailable")
        })?;
        Ok(())
    }
}
