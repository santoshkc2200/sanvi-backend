//! Durable erasure tombstones outside the database rollback boundary.
//! The shared volume is explicitly initialized by operations, never recreated silently.
use crate::{
    PrivacyConfig,
    domain::{
        AuditWriter, DataSubjectRequest, DsrKind, DsrRepository, DsrStatus, IdGenerator,
        ParticipantRepository, PrivacyError, Requester, SubjectId, SubjectRef,
    },
};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use chrono::{DateTime, Utc};
use sanvi_persistence::TxManager;
use serde::{Deserialize, Serialize};
use std::{io::Write, path::PathBuf, sync::Arc};
const INITIALIZED: &str = ".sanvi-erasure-ledger-v1";
#[derive(Serialize, Deserialize)]
struct Record {
    subject_id: SubjectId,
    erased_at: DateTime<Utc>,
    wrapped_subject: Vec<u8>,
}
struct Pending {
    subject: SubjectRef,
    erased_at: DateTime<Utc>,
}
fn storage(err: impl std::fmt::Display) -> PrivacyError {
    PrivacyError::Storage(format!("erasure ledger: {err}"))
}
fn root(directory: &str) -> Result<PathBuf, PrivacyError> {
    let path = PathBuf::from(directory);
    if !path.join(INITIALIZED).is_file() {
        return Err(storage("shared durable volume is missing or uninitialized"));
    }
    Ok(path)
}
pub fn contains(directory: &str, id: SubjectId) -> Result<bool, PrivacyError> {
    if directory.is_empty() {
        return Ok(false);
    }
    let path = root(directory)?.join(format!("{id}.json"));
    match std::fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(storage(err)),
    }
}
/// Online tenant restore cannot safely replay global erasures while traffic is live.
/// Refuse it once any durable erasure exists; use isolated recovery and replay instead.
pub fn require_safe_live_restore(directory: &str) -> Result<(), PrivacyError> {
    if directory.is_empty() {
        return Ok(());
    }
    for entry in std::fs::read_dir(root(directory)?).map_err(storage)? {
        if entry
            .map_err(storage)?
            .path()
            .extension()
            .and_then(|e| e.to_str())
            == Some("json")
        {
            return Err(storage(
                "online tenant restore is disabled after recorded erasures; restore in an isolated maintenance environment and replay erasures before serving traffic",
            ));
        }
    }
    Ok(())
}
pub fn record(
    directory: &str,
    id: SubjectId,
    wrapped_subject: Vec<u8>,
) -> Result<(), PrivacyError> {
    let path = root(directory)?;
    let destination = path.join(format!("{id}.json"));
    if destination.is_file() {
        return Ok(());
    }
    let temporary = path.join(format!(".{id}-{}.tmp", uuid::Uuid::now_v7()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(storage)?;
    let bytes = serde_json::to_vec(&Record {
        subject_id: id,
        erased_at: Utc::now(),
        wrapped_subject,
    })
    .map_err(storage)?;
    file.write_all(&bytes).map_err(storage)?;
    file.sync_all().map_err(storage)?;
    // Linking is atomic and never replaces the first durable erasure time on retries.
    match std::fs::hard_link(&temporary, &destination) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(storage(err)),
    }
    std::fs::remove_file(temporary).map_err(storage)?;
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(storage)?;
    Ok(())
}
async fn pending(tx: &TxManager, config: &PrivacyConfig) -> Result<Vec<Pending>, PrivacyError> {
    if config.erasure_ledger_dir.is_empty() {
        return Ok(Vec::new());
    }
    let root = root(&config.erasure_ledger_dir)?;
    let key = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        config.kek.expose_secret(),
    )
    .map_err(storage)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(storage)?;
    let mut result = Vec::new();
    let mut transaction = tx
        .begin_unscoped("privacy_restore_gate", "privacy")
        .await
        .map_err(storage)?;
    for entry in std::fs::read_dir(root).map_err(storage)? {
        let path = entry.map_err(storage)?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let record: Record =
            serde_json::from_slice(&std::fs::read(&path).map_err(storage)?).map_err(storage)?;
        if path.file_stem().and_then(|name| name.to_str())
            != Some(record.subject_id.to_string().as_str())
            || record.wrapped_subject.len() < 13
        {
            return Err(storage("invalid tombstone"));
        }
        let nonce_bytes: [u8; 12] = record.wrapped_subject[..12].try_into().map_err(storage)?;
        let nonce = Nonce::from(nonce_bytes);
        let reference = cipher
            .decrypt(&nonce, &record.wrapped_subject[12..])
            .map_err(|_| storage("cannot decrypt tombstone; check ledger key rotation"))?;
        let subject: SubjectRef = serde_json::from_slice(&reference).map_err(storage)?;
        let complete: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM privacy.data_subject_requests WHERE kind = 'erasure' AND status = 'completed' AND subject_ref->>'key' = $1 AND completed_at >= $2)")
    .bind(subject.canonical_key()).bind(record.erased_at).fetch_one(&mut **transaction.tx_mut().as_inner()).await.map_err(storage)?;
        if !complete {
            result.push(Pending {
                subject,
                erased_at: record.erased_at,
            });
        }
    }
    transaction.commit().await.map_err(storage)?;
    Ok(result)
}
/// The API refuses to serve a restored database until the worker replays tombstones.
pub async fn require_replayed(tx: &TxManager, config: &PrivacyConfig) -> Result<(), PrivacyError> {
    if !pending(tx, config).await?.is_empty() {
        return Err(storage(
            "restored erasures are pending; run the worker to completion before serving traffic",
        ));
    }
    Ok(())
}
/// Queue trusted, previously completed erasures after database recovery, without emailing OTPs.
pub async fn queue_restore_replay(
    tx: &TxManager,
    config: &PrivacyConfig,
    raw_ids: Arc<dyn sanvi_kernel::RawIdGenerator>,
    audit: &dyn AuditWriter,
) -> Result<usize, PrivacyError> {
    let pending = pending(tx, config).await?;
    let ids = super::misc::SystemIdGenerator::new(raw_ids.clone());
    let subjects = super::repos::SqlxSubjectRepository::new();
    let requests = super::repos::SqlxDsrRepository::new();
    let participants = super::repos::SqlxParticipantRepository::new();
    let mut queued = 0;
    for item in pending {
        let mut scope = tx
            .begin_unscoped("privacy_restore_replay", "privacy")
            .await
            .map_err(storage)?;
        let already_queued: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM privacy.data_subject_requests WHERE kind = 'erasure' AND status IN ('in_progress','partially_complete','on_hold') AND subject_ref->>'key' = $1)")
    .bind(item.subject.canonical_key()).fetch_one(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?;
        if already_queued {
            scope.commit().await.map_err(storage)?;
            continue;
        }
        let subject = crate::application::requests::ensure_subject(
            &mut scope,
            &subjects,
            &super::repos::SqlxJurisdictionRepository::new(),
            &super::misc::SystemClock,
            &ids,
            &item.subject,
            &[],
            false,
        )
        .await?;
        let now = Utc::now();
        let id = ids.next_dsr_id()?;
        let request = DataSubjectRequest {
            id,
            tenant_id: item.subject.tenant_id,
            subject_id: subject.id,
            subject_ref: item.subject,
            kind: DsrKind::Erasure,
            status: DsrStatus::InProgress,
            received_at: now,
            acknowledged_at: Some(now),
            verified_at: Some(now),
            due_at: now,
            extended_to: None,
            completed_at: None,
            jurisdiction: subject.jurisdiction,
            submitted_by: Requester::Subject,
            agent_id: None,
            requester_ip: None,
            requester_note: Some(format!(
                "Recovery replay of durable erasure recorded {}",
                item.erased_at
            )),
            rejection_reason: None,
            rejected_by: None,
            appeal_id: None,
            export_artifact_id: None,
        };
        requests.save(&mut scope, &request).await?;
        participants
            .ensure_participants(&mut scope, id, &crate::domain::CONTEXT_NAMES)
            .await?;
        audit
            .record(
                &mut scope,
                None,
                "privacy.restore.erasure_replay".to_string(),
                id.to_string(),
                serde_json::json!({"erased_at": item.erased_at}),
            )
            .await?;
        scope.commit().await.map_err(storage)?;
        queued += 1;
    }
    Ok(queued)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_volume_fails_closed_and_tombstones_survive_a_new_reader() {
        let directory =
            std::env::temp_dir().join(format!("sanvi-erasure-ledger-{}", uuid::Uuid::now_v7()));
        let name = directory.to_str().unwrap();
        let id = SubjectId::try_from(123_i64).unwrap();
        assert!(contains(name, id).is_err());
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(INITIALIZED), b"v1").unwrap();
        require_safe_live_restore(name).unwrap();
        record(name, id, vec![7; 64]).unwrap();
        assert!(require_safe_live_restore(name).is_err());
        let before = std::fs::read(directory.join("123.json")).unwrap();
        record(name, id, vec![9; 64]).unwrap();
        assert_eq!(std::fs::read(directory.join("123.json")).unwrap(), before);
        assert!(contains(name, id).unwrap());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
