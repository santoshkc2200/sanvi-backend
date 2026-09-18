//! The nightly chain verification: page through the whole audit trail and
//! recompute the hash chain. Any mismatch is a security incident — alert
//! immediately (the worker job logs at error level and bumps the
//! `audit_chain_verify_failures` metric).

use std::sync::Arc;

use sanvi_persistence::TxManager;

use crate::application::storage;
use crate::domain::{AuditCursor, AuditError, AuditReader, verify_segment};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainVerificationReport {
    pub entries_checked: u64,
    pub verified: bool,
    /// First mismatch (entry id + position), when `verified` is false.
    pub first_mismatch: Option<String>,
}

pub struct VerifyAuditChain {
    tx_manager: TxManager,
    reader: Arc<dyn AuditReader>,
    batch_size: u32,
}

impl VerifyAuditChain {
    pub fn new(tx_manager: TxManager, reader: Arc<dyn AuditReader>, batch_size: u32) -> Self {
        Self {
            tx_manager,
            reader,
            batch_size,
        }
    }

    /// Verify the whole chain from genesis. Paged so an arbitrarily long
    /// trail never loads into memory at once; the per-page verification
    /// carries the running hash across pages by re-reading page boundaries.
    #[tracing::instrument(name = "audit.verify_audit_chain", skip_all)]
    pub async fn execute(&self) -> Result<ChainVerificationReport, AuditError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("audit_verify", "audit")
            .await
            .map_err(storage)?;

        // Page in chain order (ascending seq), so the running hash
        // continues naturally across pages.
        let mut checked: u64 = 0;
        let mut running_hash: Option<[u8; 32]> = None;
        let mut after: Option<AuditCursor> = None;
        loop {
            let (entries, _) = self
                .reader
                .list(
                    &mut scope,
                    &Default::default(),
                    after,
                    self.batch_size,
                    true,
                )
                .await?;
            if entries.is_empty() {
                break;
            }
            checked += entries.len() as u64;
            let Some(newest) = entries.last() else {
                break;
            };
            after = Some(AuditCursor { seq: newest.seq });
            // The true first page starts at genesis; every later page
            // continues from the previous page's last hash.
            let previous = running_hash.unwrap_or(crate::domain::GENESIS_HASH);
            match verify_segment(&entries, previous) {
                Ok(last_hash) => running_hash = Some(last_hash),
                Err(err) => {
                    return Ok(ChainVerificationReport {
                        entries_checked: checked,
                        verified: false,
                        first_mismatch: Some(err.to_string()),
                    });
                }
            }
            if entries.len() < self.batch_size as usize {
                break;
            }
        }

        Ok(ChainVerificationReport {
            entries_checked: checked,
            verified: true,
            first_mismatch: None,
        })
    }
}
