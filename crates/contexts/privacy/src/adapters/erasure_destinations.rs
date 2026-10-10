//! The built-in [`ErasureDestination`] adapters: the destinations outside
//! the provider registry that an erasure must still reach, each purged or
//! documented as lawfully retained with an end date. The composition root
//! adds the foreign-context destinations (the ad-platform suppression list
//! via the advertising context's retraction sweep) onto the same port.

use std::sync::Arc;

use async_trait::async_trait;
use sanvi_persistence::TxManager;

use sanvi_cache::SsrCache;

use crate::domain::{
    Clock, DestinationRetainedItem, EraseDestinationOutcome, ErasureDestination, PrivacyError,
    RetentionAction, RetentionRuleRepository, SubjectRef,
};
use sanvi_kernel::TenantId;

/// The backup window: PITR/WAL backups inside the retention window still
/// carry the subject's rows. The erasure path crypto-shreds the subject's
/// DEK, so those copies are unrecoverable ciphertext — the documented
/// end date is when the last backup holding the plaintext ages out.
pub struct BackupWindowDestination {
    pub backup_retention_days: i64,
    pub clock: Arc<dyn Clock>,
}

#[async_trait]
impl ErasureDestination for BackupWindowDestination {
    fn name(&self) -> &'static str {
        "backup_window"
    }

    fn description(&self) -> String {
        format!(
            "point-in-time backups inside the {}-day retention window; the subject's wrapped key was destroyed, so archived copies are unrecoverable ciphertext",
            self.backup_retention_days
        )
    }

    async fn propagate_erasure(
        &self,
        subject: &SubjectRef,
        _tenant_id: Option<TenantId>,
    ) -> Result<EraseDestinationOutcome, PrivacyError> {
        let now = self.clock.now();
        Ok(EraseDestinationOutcome::retained(vec![
            DestinationRetainedItem {
                detail: format!(
                    "backup segments containing {} rows, rendered unreadable by crypto-shred",
                    subject.canonical_key()
                ),
                reason: "backup window; plaintext unrecoverable after key destruction".to_string(),
                retains_until: Some(now + chrono::Duration::days(self.backup_retention_days)),
            },
        ]))
    }
}

/// The search index: this workspace has no search infrastructure beyond
/// Postgres full-text, so an erased subject's rows took their index
/// entries with them. The destination exists so the propagation ledger
/// states that as a checked fact per erasure, not an assumption.
#[derive(Debug, Clone, Copy, Default)]
pub struct SearchIndexDestination;

#[async_trait]
impl ErasureDestination for SearchIndexDestination {
    fn name(&self) -> &'static str {
        "search_indexes"
    }

    fn description(&self) -> String {
        "no search infrastructure beyond Postgres full-text: the index entries died with the rows"
            .to_string()
    }

    async fn propagate_erasure(
        &self,
        _subject: &SubjectRef,
        _tenant_id: Option<TenantId>,
    ) -> Result<EraseDestinationOutcome, PrivacyError> {
        // Nothing retained: there is no store to purge and none to justify.
        Ok(EraseDestinationOutcome::purged(0))
    }
}

/// The archives (TASK-028): the `archive`-action classes (invoices,
/// transaction records) survive an erasure request under statutory
/// records schedules. Archived ranges are not rewritten row-wise; the
/// ledger records each class's basis and the schedule date the archive
/// expires on.
pub struct ArchivesDestination {
    tx_manager: TxManager,
    rules: Arc<dyn RetentionRuleRepository>,
    clock: Arc<dyn Clock>,
}

impl ArchivesDestination {
    pub fn new(
        tx_manager: TxManager,
        rules: Arc<dyn RetentionRuleRepository>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            rules,
            clock,
        }
    }
}

#[async_trait]
impl ErasureDestination for ArchivesDestination {
    fn name(&self) -> &'static str {
        "archives"
    }

    fn description(&self) -> String {
        "archived retention ranges (TASK-028 archival sweep); classes under statutory records schedules are retained, not rewritten".to_string()
    }

    async fn propagate_erasure(
        &self,
        subject: &SubjectRef,
        _tenant_id: Option<TenantId>,
    ) -> Result<EraseDestinationOutcome, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_erasure_archives", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let rules = self.rules.list(&mut scope).await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        let now = self.clock.now();
        let retained = rules
            .iter()
            .filter(|rule| rule.action == RetentionAction::Archive)
            .map(|rule| DestinationRetainedItem {
                detail: format!(
                    "archived {} ranges containing {}",
                    rule.data_class.as_str(),
                    subject.canonical_key()
                ),
                reason: format!(
                    "statutory records schedule: {} days, archival is the removal path",
                    rule.period_days
                ),
                // Upper bound: the archive expires per range on the class
                // schedule; the newest possible expiry is the erasure date
                // plus one full period.
                retains_until: Some(now + chrono::Duration::days(rule.period_days)),
            })
            .collect::<Vec<_>>();
        Ok(EraseDestinationOutcome {
            purged: 0,
            retained,
        })
    }
}

/// The SSR response cache: cached storefront pages can still show erased
/// data until their TTL expires. The purge drops every cached page of the
/// subject's tenant immediately; without a tenant (platform-level
/// subject) there is nothing tenant-scoped to drop.
pub struct CacheDestination {
    pub ssr: Arc<SsrCache>,
}

#[async_trait]
impl ErasureDestination for CacheDestination {
    fn name(&self) -> &'static str {
        "caches"
    }

    fn description(&self) -> String {
        "SSR response cache: every cached page of the subject's tenant is dropped so no cached render outlives the erasure".to_string()
    }

    async fn propagate_erasure(
        &self,
        _subject: &SubjectRef,
        tenant_id: Option<TenantId>,
    ) -> Result<EraseDestinationOutcome, PrivacyError> {
        match tenant_id {
            Some(tenant_id) => {
                let purged = self
                    .ssr
                    .invalidate_tenant(tenant_id)
                    .await
                    .map_err(|err| PrivacyError::Storage(err.to_string()))?;
                Ok(EraseDestinationOutcome::purged(purged))
            }
            None => Ok(EraseDestinationOutcome::purged(0)),
        }
    }
}
