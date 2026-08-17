//! Local filesystem storage for brand assets and published artifacts.
//!
//! Layout:
//! - Brand assets: `{root}/brand/{tenant_id}/{digest}.{ext}`
//! - Artifacts:    `{root}/artifacts/{artifact_uri}`
//!
//! Content addressing (`digest` in the file name) makes uploads idempotent
//! and immune to overwrite attacks. Production deployments point a CDN or
//! object store at the same layout; the port stays the same.

use async_trait::async_trait;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::domain::{ArtifactStore, BrandAssetStore, ThemingError};

/// Build a file-store error from any io failure.
fn store_err(err: impl std::fmt::Display) -> ThemingError {
    ThemingError::Storage(format!("asset store failure: {err}"))
}

#[derive(Debug, Clone)]
pub struct LocalFileStore {
    root: PathBuf,
}

impl LocalFileStore {
    /// Create the store for a root directory (created lazily on write).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn brand_path(&self, tenant_id: Uuid, digest: &str, ext: &str) -> PathBuf {
        self.root
            .join("brand")
            .join(tenant_id.to_string())
            .join(format!("{digest}.{ext}"))
    }

    fn artifact_path(&self, key: &str) -> Result<PathBuf, ThemingError> {
        let path = self.root.join("artifacts").join(key);
        // The artifact key is validated at the call sites; this defence
        // makes traversal structurally impossible from this adapter.
        let canonical_root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        let normalized = path
            .components()
            .all(|component| !matches!(component, std::path::Component::ParentDir));
        if !normalized || !path.starts_with(&canonical_root) {
            return Err(ThemingError::Storage(
                "artifact key escapes the artifact root".to_string(),
            ));
        }
        Ok(path)
    }

    async fn write_file(&self, path: &Path, bytes: &[u8]) -> Result<(), ThemingError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(store_err)?;
        }
        tokio::fs::write(path, bytes).await.map_err(store_err)
    }
}

#[async_trait]
impl BrandAssetStore for LocalFileStore {
    async fn put(
        &self,
        tenant_id: Uuid,
        digest: &str,
        ext: &str,
        bytes: &[u8],
    ) -> Result<(), ThemingError> {
        let path = self.brand_path(tenant_id, digest, ext);
        self.write_file(&path, bytes).await
    }

    async fn delete(&self, tenant_id: Uuid, digest: &str, ext: &str) -> Result<(), ThemingError> {
        let path = self.brand_path(tenant_id, digest, ext);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(store_err(err)),
        }
    }
}

#[async_trait]
impl ArtifactStore for LocalFileStore {
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), ThemingError> {
        let path = self.artifact_path(key)?;
        self.write_file(&path, bytes).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("sanvi-theming-test-{}", uuid::Uuid::now_v7()))
    }

    #[tokio::test]
    async fn stores_and_deletes_brand_assets() {
        let root = temp_root();
        let store = LocalFileStore::new(root.clone());
        let tenant = Uuid::now_v7();
        let digest = "a".repeat(64);
        <LocalFileStore as BrandAssetStore>::put(&store, tenant, &digest, "png", b"data")
            .await
            .unwrap();
        let path = root
            .join("brand")
            .join(tenant.to_string())
            .join(format!("{digest}.png"));
        assert!(path.exists());
        <LocalFileStore as BrandAssetStore>::delete(&store, tenant, &digest, "png")
            .await
            .unwrap();
        assert!(!path.exists());
        // Deleting again is a no-op, not an error.
        <LocalFileStore as BrandAssetStore>::delete(&store, tenant, &digest, "png")
            .await
            .unwrap();
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn rejects_artifact_traversal() {
        let root = temp_root();
        let store = LocalFileStore::new(root.clone());
        assert!(
            <LocalFileStore as ArtifactStore>::put(&store, "../escape.zip", b"x")
                .await
                .is_err()
        );
        let _ = tokio::fs::remove_dir_all(&root).await;
    }
}
