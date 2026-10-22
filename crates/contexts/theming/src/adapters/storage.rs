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
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

use sanvi_kernel::TenantId;

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

    fn brand_path(&self, tenant_id: TenantId, digest: &str, ext: &str) -> PathBuf {
        self.root
            .join("brand")
            .join(tenant_id.to_string())
            .join(format!("{digest}.{ext}"))
    }

    fn artifact_path(&self, key: &str) -> Result<PathBuf, ThemingError> {
        let key_path = Path::new(key);
        let normalized = !key_path.as_os_str().is_empty()
            && key_path
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)));
        if !normalized {
            return Err(ThemingError::Storage(
                "artifact key escapes the artifact root".to_string(),
            ));
        }
        Ok(self.root.join("artifacts").join(key_path))
    }

    async fn write_file(&self, path: &Path, bytes: &[u8]) -> Result<(), ThemingError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(store_err)?;
        }
        tokio::fs::write(path, bytes).await.map_err(store_err)
    }

    async fn write_immutable_file(&self, path: &Path, bytes: &[u8]) -> Result<(), ThemingError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(store_err)?;
        }
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await
        {
            Ok(mut file) => {
                file.write_all(bytes).await.map_err(store_err)?;
                // Tokio can finish write_all after buffering the write.
                // Complete its filesystem operation before publishing success.
                file.flush().await.map_err(store_err)
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = tokio::fs::read(path).await.map_err(store_err)?;
                if existing == bytes {
                    Ok(())
                } else {
                    Err(ThemingError::Storage(
                        "refusing to overwrite an immutable theme artifact".to_string(),
                    ))
                }
            }
            Err(err) => Err(store_err(err)),
        }
    }
}

#[async_trait]
impl BrandAssetStore for LocalFileStore {
    async fn put(
        &self,
        tenant_id: TenantId,
        digest: &str,
        ext: &str,
        bytes: &[u8],
    ) -> Result<(), ThemingError> {
        let path = self.brand_path(tenant_id, digest, ext);
        self.write_file(&path, bytes).await
    }

    async fn delete(
        &self,
        tenant_id: TenantId,
        digest: &str,
        ext: &str,
    ) -> Result<(), ThemingError> {
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
        self.write_immutable_file(&path, bytes).await
    }

    async fn verify(&self, key: &str, digest: &str) -> Result<(), ThemingError> {
        let path = self.artifact_path(key)?;
        let bytes = tokio::fs::read(path).await.map_err(store_err)?;
        let actual = hex::encode(Sha256::digest(&bytes));
        if actual == digest {
            Ok(())
        } else {
            Err(ThemingError::Storage(format!(
                "artifact digest mismatch: expected {digest}, got {actual}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        // nosemgrep: rust.lang.security.temp-dir.temp-dir — uuid-unique test fixture, not a security operation
        std::env::temp_dir().join(format!("sanvi-theming-test-{}", uuid::Uuid::now_v7()))
    }

    #[tokio::test]
    async fn stores_and_deletes_brand_assets() {
        let root = temp_root();
        let store = LocalFileStore::new(root.clone());
        let tenant = TenantId::try_from(1_000_001_i64).unwrap();
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

    #[tokio::test]
    async fn relative_root_supports_multiple_artifact_writes() {
        let relative = PathBuf::from(format!(
            "target/sanvi-theming-test-{}",
            uuid::Uuid::now_v7()
        ));
        let store = LocalFileStore::new(relative.clone());
        <LocalFileStore as ArtifactStore>::put(&store, "first.zip", b"first")
            .await
            .unwrap();
        <LocalFileStore as ArtifactStore>::put(&store, "second.zip", b"second")
            .await
            .unwrap();
        assert!(relative.join("artifacts/first.zip").exists());
        assert!(relative.join("artifacts/second.zip").exists());
        let _ = tokio::fs::remove_dir_all(&relative).await;
    }

    #[tokio::test]
    async fn artifacts_are_immutable_and_digest_verified() {
        let root = temp_root();
        let store = LocalFileStore::new(root.clone());
        let digest = hex::encode(Sha256::digest(b"original"));
        <LocalFileStore as ArtifactStore>::put(&store, "artifact.zip", b"original")
            .await
            .unwrap();
        <LocalFileStore as ArtifactStore>::verify(&store, "artifact.zip", &digest)
            .await
            .unwrap();
        assert!(
            <LocalFileStore as ArtifactStore>::put(&store, "artifact.zip", b"replacement")
                .await
                .is_err()
        );
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[test]
    fn completed_artifact_put_is_visible_to_an_immediate_filesystem_reader() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let root = temp_root();
            let store = LocalFileStore::new(root.clone());
            let bytes = vec![0x5a; 16 * 1024];
            let mut exercised = false;
            for index in 0..128 {
                let key = format!("completed-{index}.zip");
                let path = root.join("artifacts").join(&key);
                let mut put =
                    Box::pin(<LocalFileStore as ArtifactStore>::put(&store, &key, &bytes));
                // Stop polling as soon as create_new has made the pathname
                // visible, before the async open resumes to submit its write.
                let completed = loop {
                    if path.exists() {
                        break false;
                    }
                    if let std::task::Poll::Ready(result) = futures_util::poll!(put.as_mut()) {
                        result.unwrap();
                        break true;
                    }
                    tokio::task::yield_now().await;
                };
                if completed {
                    continue;
                }
                let (started_tx, started_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                let blocker = tokio::task::spawn_blocking(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                });
                started_rx.await.unwrap();
                // The only blocking worker is occupied. A write can be
                // accepted into Tokio's buffer, but cannot have completed.
                let premature = match futures_util::poll!(put.as_mut()) {
                    std::task::Poll::Ready(result) => {
                        result.unwrap();
                        true
                    }
                    std::task::Poll::Pending => false,
                };
                release_tx.send(()).unwrap();
                blocker.await.unwrap();
                assert!(
                    !premature,
                    "put returned while its filesystem write was still queued"
                );
                put.await.unwrap();
                let actual = std::fs::read(path).unwrap();
                assert_eq!(actual, bytes);
                exercised = true;
                break;
            }
            assert!(
                exercised,
                "failed to intercept the independent filesystem operation"
            );
            let _ = tokio::fs::remove_dir_all(&root).await;
        });
    }
}
