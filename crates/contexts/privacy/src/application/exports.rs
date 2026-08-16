//! Export artifact handling: packaging happens in the DSR saga; this
//! module owns the one-time download path (token + passphrase + TTL + max
//! downloads) and the expiry cleanup job.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use sanvi_persistence::TxManager;

use crate::domain::{
    Clock, DsrRepository, EnvelopeCrypto, ExportArtifact, ExportArtifactRepository, Hasher,
    PrivacyError,
};

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct DownloadExportCommand {
    pub request_id: Uuid,
    pub token: String,
    pub passphrase: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ExportDownloadView {
    pub artifact_id: Uuid,
    pub content: Vec<u8>,
    pub manifest_sha256: String,
}

pub struct DownloadExport {
    tx_manager: TxManager,
    requests: Arc<dyn DsrRepository>,
    artifacts: Arc<dyn ExportArtifactRepository>,
    hasher: Arc<dyn Hasher>,
    clock: Arc<dyn Clock>,
}

impl DownloadExport {
    pub fn new(
        tx_manager: TxManager,
        requests: Arc<dyn DsrRepository>,
        artifacts: Arc<dyn ExportArtifactRepository>,
        hasher: Arc<dyn Hasher>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            requests,
            artifacts,
            hasher,
            clock,
        }
    }

    /// Validate token, passphrase, TTL and the download cap; then read the
    /// (encrypted) archive from the artifact directory and return it.
    pub async fn execute(
        &self,
        command: DownloadExportCommand,
    ) -> Result<ExportDownloadView, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_export_download", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let request = self
            .requests
            .find_by_id(
                &mut scope,
                crate::domain::DsrId::from_uuid(command.request_id),
            )
            .await?
            .ok_or(PrivacyError::ArtifactNotFound(
                command.request_id.to_string(),
            ))?;
        let artifact_id = request
            .export_artifact_id
            .ok_or(PrivacyError::ArtifactNotFound(
                command.request_id.to_string(),
            ))?;
        let artifact = self.artifacts.find(&mut scope, artifact_id).await?.ok_or(
            PrivacyError::ArtifactNotFound(command.request_id.to_string()),
        )?;
        // Enumeration defence: the token is a high-entropy bearer secret;
        // the hash comparison is constant-time (HMAC pepper).
        if !self
            .hasher
            .verify(&command.token, &artifact.download_token_hash)
        {
            return Err(PrivacyError::ArtifactNotFound(
                command.request_id.to_string(),
            ));
        }
        if !self
            .hasher
            .verify(&command.passphrase, &artifact.passphrase_verifier)
        {
            return Err(PrivacyError::ArtifactPassphraseMismatch);
        }
        if artifact.expires_at < self.clock.now() {
            return Err(PrivacyError::ArtifactExpired);
        }
        if artifact.download_count >= artifact.max_downloads {
            return Err(PrivacyError::ArtifactDownloadsExhausted);
        }

        let content = tokio::fs::read(&artifact.path)
            .await
            .map_err(|err| PrivacyError::Storage(format!("read artifact: {err}")))?;

        self.artifacts
            .record_download(&mut scope, artifact.id)
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        Ok(ExportDownloadView {
            artifact_id: artifact.id,
            content,
            manifest_sha256: artifact.manifest_sha256,
        })
    }
}

/// Deletes expired artifacts (row + file). Runs as a worker job; the TTL
/// starts at packaging time.
pub struct CleanupExpiredExports {
    tx_manager: TxManager,
    artifacts: Arc<dyn ExportArtifactRepository>,
}

impl CleanupExpiredExports {
    pub fn new(tx_manager: TxManager, artifacts: Arc<dyn ExportArtifactRepository>) -> Self {
        Self {
            tx_manager,
            artifacts,
        }
    }

    pub async fn execute(&self, limit: u32) -> Result<usize, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_export_cleanup", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let expired = self.artifacts.expired(&mut scope, limit.min(500)).await?;
        let mut removed = 0;
        for artifact in expired {
            if tokio::fs::remove_file(&artifact.path).await.is_ok() {
                removed += 1;
            }
            self.artifacts.delete(&mut scope, artifact.id).await?;
        }
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(removed)
    }
}

/// The artifact path builder (shared with the saga).
pub fn artifact_path(export_dir: &str, artifact_id: Uuid, now: DateTime<Utc>) -> String {
    format!(
        "{}/{}-{}.enc",
        export_dir.trim_end_matches('/'),
        now.format("%Y%m%d"),
        artifact_id
    )
}

/// Persist a packaged artifact (shared with the saga).
#[allow(clippy::too_many_arguments)]
pub async fn store_artifact(
    scope: &mut sanvi_persistence::PlatformScope<'_>,
    artifacts: &dyn ExportArtifactRepository,
    hasher: &dyn Hasher,
    crypto: &dyn EnvelopeCrypto,
    request_id: crate::domain::DsrId,
    subject_id: crate::domain::SubjectId,
    archive: Vec<u8>,
    passphrase: &str,
    token: &str,
    export_dir: &str,
    ttl_days: i64,
    max_downloads: i32,
    now: DateTime<Utc>,
) -> Result<ExportArtifact, PrivacyError> {
    let encrypted = crypto.encrypt_with_passphrase(&archive, passphrase)?;
    let id = Uuid::now_v7();
    let path = artifact_path(export_dir, id, now);
    if let Some(parent) = std::path::Path::new(&path).parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|err| PrivacyError::Storage(format!("artifact dir: {err}")))?;
    }
    tokio::fs::write(&path, &encrypted)
        .await
        .map_err(|err| PrivacyError::Storage(format!("write artifact: {err}")))?;
    let artifact = ExportArtifact {
        id,
        request_id,
        subject_id,
        download_token_hash: hasher.hash(token),
        passphrase_verifier: hasher.hash(passphrase),
        manifest_sha256: crate::adapters::archive_sha256(&archive),
        path,
        content_size: encrypted.len() as i64,
        expires_at: now + chrono::Duration::days(ttl_days),
        max_downloads,
        download_count: 0,
        downloaded_at: None,
        created_at: now,
    };
    artifacts.save(scope, &artifact).await?;
    Ok(artifact)
}
