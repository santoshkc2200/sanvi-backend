use crate::domain::{MediaError, MediaOwnerIndexPort};
use async_trait::async_trait;
use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;

pub struct SqlxMediaOwnerIndex(TxManager);
impl SqlxMediaOwnerIndex {
    pub fn new(tx: TxManager) -> Self {
        Self(tx)
    }
}
#[async_trait]
impl MediaOwnerIndexPort for SqlxMediaOwnerIndex {
    async fn remember(&self, tenant_id: TenantId, owner: &str) -> Result<(), MediaError> {
        let subject_key = if owner.starts_with("email:") {
            owner.to_ascii_lowercase()
        } else if owner.starts_with("user:") || owner.starts_with(&format!("end_user:{tenant_id}:"))
        {
            owner.to_string()
        } else {
            format!("end_user:{tenant_id}:{owner}")
        };
        let unavailable = || MediaError::Unavailable {
            detail: "media ownership storage unavailable".to_string(),
        };
        let mut scope = self
            .0
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|_| unavailable())?;
        sqlx::query("INSERT INTO media.subject_namespaces(subject_key,tenant_id,owner_ref) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(subject_key).bind(tenant_id.as_i64()).bind(owner).execute(&mut **scope.tx_mut().as_inner()).await.map_err(|_| unavailable())?;
        scope.commit().await.map_err(|_| unavailable())
    }
}
