//! Retention: rules are database rows (counsel can change periods without
//! a release); the nightly sweep reports in dry-run mode and deletes /
//! anonymises for real only when configured. The published retention table
//! is generated from the same rules.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use sanvi_kernel::UserId;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use sanvi_persistence::TxManager;

use crate::domain::{
    AuditWriter, Clock, ConsentRepository, PrivacyError, RetentionAction, RetentionRule,
    RetentionRuleRepository, SignalRepository,
};

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RetentionSweepReport {
    pub dry_run: bool,
    pub started_at: DateTime<Utc>,
    pub scanned: RetentionCounts,
    pub would_delete: RetentionCounts,
    pub deleted: RetentionCounts,
}

#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct RetentionCounts {
    pub consents: usize,
    pub preference_signals: usize,
    pub anonymised: usize,
}

pub struct RetentionSweep {
    tx_manager: TxManager,
    rules: Arc<dyn RetentionRuleRepository>,
    consents: Arc<dyn ConsentRepository>,
    signals: Arc<dyn SignalRepository>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditWriter>,
    dry_run: bool,
    batch_size: u32,
}

impl RetentionSweep {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx_manager: TxManager,
        rules: Arc<dyn RetentionRuleRepository>,
        consents: Arc<dyn ConsentRepository>,
        signals: Arc<dyn SignalRepository>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditWriter>,
        dry_run: bool,
        batch_size: u32,
    ) -> Self {
        Self {
            tx_manager,
            rules,
            consents,
            signals,
            clock,
            audit,
            dry_run,
            batch_size,
        }
    }

    #[tracing::instrument(name = "privacy.retention_sweep", skip_all)]
    pub async fn execute(&self) -> Result<RetentionSweepReport, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_retention_sweep", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let rules = self.rules.list(&mut scope).await?;

        let consent_days = rules
            .iter()
            .find(|rule| rule.data_class == crate::domain::DataClass::ConsentRecord)
            .map(|rule| rule.period_days)
            .unwrap_or(3650);
        let signal_days = rules
            .iter()
            .find(|rule| rule.data_class == crate::domain::DataClass::Preference)
            .map(|rule| rule.period_days)
            .unwrap_or(365);

        let now = self.clock.now();
        let consent_cutoff = now - Duration::days(consent_days);
        let signal_cutoff = now - Duration::days(signal_days);

        let expired_consents = self
            .consents
            .expired(&mut scope, consent_cutoff, self.batch_size)
            .await?;
        let expired_signals = self
            .signals
            .expired(&mut scope, signal_cutoff, self.batch_size)
            .await?;

        let scanned = RetentionCounts {
            consents: expired_consents.len(),
            preference_signals: expired_signals.len(),
            anonymised: 0,
        };
        let would_delete = RetentionCounts {
            consents: expired_consents.len(),
            preference_signals: expired_signals.len(),
            anonymised: 0,
        };
        let mut deleted = RetentionCounts::default();

        if !self.dry_run {
            let consent_action = rules
                .iter()
                .find(|rule| rule.data_class == crate::domain::DataClass::ConsentRecord)
                .map(|rule| rule.action)
                .unwrap_or(RetentionAction::Anonymise);
            match consent_action {
                RetentionAction::Delete => {
                    for record in expired_consents {
                        self.consents.delete(&mut scope, record.id).await?;
                        deleted.consents += 1;
                    }
                }
                RetentionAction::Anonymise => {
                    for record in expired_consents {
                        let anonymised =
                            crate::adapters::anonymised_subject_ref(&record.subject_ref);
                        self.consents
                            .anonymise_record(&mut scope, record.id, &anonymised)
                            .await?;
                        deleted.anonymised += 1;
                    }
                }
                RetentionAction::Archive => {
                    deleted.anonymised = 0; // archive is a storage-tier move, out of scope
                }
            }
            for signal in expired_signals {
                self.signals.delete(&mut scope, signal.id).await?;
                deleted.preference_signals += 1;
            }
            self.audit
                .record(
                    &mut scope,
                    None,
                    "privacy.retention.sweep".to_string(),
                    "retention_sweep".to_string(),
                    serde_json::to_value(&deleted).unwrap_or_default(),
                )
                .await?;
        }

        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        Ok(RetentionSweepReport {
            dry_run: self.dry_run,
            started_at: now,
            scanned,
            would_delete,
            deleted,
        })
    }
}

pub struct ListRetentionRules {
    tx_manager: TxManager,
    rules: Arc<dyn RetentionRuleRepository>,
}

impl ListRetentionRules {
    pub fn new(tx_manager: TxManager, rules: Arc<dyn RetentionRuleRepository>) -> Self {
        Self { tx_manager, rules }
    }

    #[tracing::instrument(name = "privacy.list_retention_rules", skip_all)]
    pub async fn execute(&self) -> Result<Vec<RetentionRule>, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_retention_list", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let rules = self.rules.list(&mut scope).await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(rules)
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpsertRetentionRuleCommand {
    pub data_class: String,
    pub sensitivity: String,
    pub basis: Option<String>,
    pub period_days: i64,
    pub action: String,
    pub disclosed_in_notice: bool,
}

pub struct UpsertRetentionRule {
    tx_manager: TxManager,
    rules: Arc<dyn RetentionRuleRepository>,
    audit: Arc<dyn AuditWriter>,
}

impl UpsertRetentionRule {
    pub fn new(
        tx_manager: TxManager,
        rules: Arc<dyn RetentionRuleRepository>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            rules,
            audit,
        }
    }

    #[tracing::instrument(name = "privacy.upsert_retention_rule", skip_all)]
    pub async fn execute(
        &self,
        actor: UserId,
        command: UpsertRetentionRuleCommand,
    ) -> Result<(), PrivacyError> {
        let rule = RetentionRule {
            data_class: crate::domain::DataClass::parse(&command.data_class)?,
            sensitivity: crate::domain::SensitivityClass::parse(&command.sensitivity)?,
            basis: command
                .basis
                .as_deref()
                .map(crate::domain::LegalBasis::parse)
                .transpose()?,
            period_days: command.period_days,
            action: RetentionAction::parse(&command.action)?,
            disclosed_in_notice: command.disclosed_in_notice,
        };
        if rule.period_days <= 0 {
            return Err(PrivacyError::Conflict(
                "period_days must be positive".to_string(),
            ));
        }
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_retention_upsert", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        self.rules.upsert(&mut scope, &rule, actor).await?;
        self.audit
            .record(
                &mut scope,
                Some(actor),
                "privacy.retention.update".to_string(),
                rule.data_class.as_str().to_string(),
                serde_json::to_value(&rule).unwrap_or_default(),
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(())
    }
}
