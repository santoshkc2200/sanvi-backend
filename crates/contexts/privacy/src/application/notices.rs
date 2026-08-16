//! Public notice views: generated from live configuration — the notice
//! version, the retention table (from `privacy.retention_rules`), the
//! sub-processor list and the jurisdiction-specific sections. Nothing here
//! is hand-written content; phase 06 localises the strings.

use std::sync::Arc;

use serde::Serialize;
use utoipa::ToSchema;

use sanvi_persistence::TxManager;

use crate::domain::{
    JurisdictionRepository, PrivacyError, RetentionRuleRepository, SubProcessorRepository,
};

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PrivacyNoticeView {
    pub notice_version: String,
    pub jurisdictions: Vec<NoticeJurisdictionView>,
    pub retention: Vec<RetentionNoticeRow>,
    pub subprocessors: Vec<SubProcessorNoticeRow>,
    pub backup_retention_days: i64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NoticeJurisdictionView {
    pub code: String,
    pub regime: String,
    pub consent_model: String,
    pub response_days: u16,
    pub appeal_window_days: Option<u16>,
    pub honours_universal_opt_out: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RetentionNoticeRow {
    pub category: String,
    pub period_days: i64,
    pub action: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SubProcessorNoticeRow {
    pub name: String,
    pub purpose: String,
    pub location: String,
    pub role: String,
    pub transfer_mechanism: Option<String>,
}

pub struct NoticeService {
    tx_manager: TxManager,
    rules: Arc<dyn RetentionRuleRepository>,
    processors: Arc<dyn SubProcessorRepository>,
    jurisdictions: Arc<dyn JurisdictionRepository>,
}

impl NoticeService {
    pub fn new(
        tx_manager: TxManager,
        rules: Arc<dyn RetentionRuleRepository>,
        processors: Arc<dyn SubProcessorRepository>,
        jurisdictions: Arc<dyn JurisdictionRepository>,
    ) -> Self {
        Self {
            tx_manager,
            rules,
            processors,
            jurisdictions,
        }
    }

    pub async fn build(
        &self,
        notice_version: &str,
        backup_retention_days: i64,
    ) -> Result<PrivacyNoticeView, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_notice", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let rules = self.rules.list(&mut scope).await?;
        let processors = self.processors.list_public(&mut scope).await?;
        let profiles = self.jurisdictions.list(&mut scope).await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        Ok(PrivacyNoticeView {
            notice_version: notice_version.to_string(),
            jurisdictions: profiles
                .into_iter()
                .map(|profile| NoticeJurisdictionView {
                    code: profile.code.as_str().to_string(),
                    regime: profile.regime.as_str().to_string(),
                    consent_model: profile.consent_model.as_str().to_string(),
                    response_days: profile.response_days,
                    appeal_window_days: profile.appeal_window_days,
                    honours_universal_opt_out: profile.honours_universal_opt_out,
                })
                .collect(),
            retention: rules
                .iter()
                .filter(|rule| rule.disclosed_in_notice)
                .map(|rule| RetentionNoticeRow {
                    category: crate::domain::DATA_CLASSES
                        .iter()
                        .find(|def| def.class == rule.data_class)
                        .map(|def| def.category)
                        .unwrap_or(rule.data_class.as_str())
                        .to_string(),
                    period_days: rule.period_days,
                    action: rule.action.as_str().to_string(),
                })
                .collect(),
            subprocessors: processors
                .into_iter()
                .map(|processor| SubProcessorNoticeRow {
                    name: processor.name,
                    purpose: processor.purpose,
                    location: processor.location,
                    role: processor.role.as_str().to_string(),
                    transfer_mechanism: processor
                        .transfer_mechanism
                        .map(|m| m.as_str().to_string()),
                })
                .collect(),
            backup_retention_days,
        })
    }
}

/// The notice at collection (served before any collection happens): the
/// categories collected, the purposes, sale/share status and retention
/// summary.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NoticeAtCollectionView {
    pub notice_version: String,
    pub categories: Vec<String>,
    pub purposes: Vec<String>,
    pub sale_or_share: bool,
    pub targeted_advertising: bool,
    pub sensitive_pi: bool,
    pub retention_summary: Vec<RetentionNoticeRow>,
    pub opt_out_effective_business_days: Option<u16>,
}

impl NoticeAtCollectionView {
    pub fn build(notice_version: &str, rules: &[crate::domain::RetentionRule]) -> Self {
        let categories: Vec<String> = crate::domain::DATA_CLASSES
            .iter()
            .map(|def| def.category.to_string())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let sensitive = rules
            .iter()
            .any(|rule| rule.sensitivity.requires_sensitive_handling());
        Self {
            notice_version: notice_version.to_string(),
            categories,
            purposes: crate::domain::ProcessingPurpose::ALL
                .iter()
                .map(|purpose| purpose.as_str().to_string())
                .collect(),
            sale_or_share: false,
            targeted_advertising: false,
            sensitive_pi: sensitive,
            retention_summary: rules
                .iter()
                .filter(|rule| rule.disclosed_in_notice)
                .map(|rule| RetentionNoticeRow {
                    category: crate::domain::DATA_CLASSES
                        .iter()
                        .find(|def| def.class == rule.data_class)
                        .map(|def| def.category)
                        .unwrap_or(rule.data_class.as_str())
                        .to_string(),
                    period_days: rule.period_days,
                    action: rule.action.as_str().to_string(),
                })
                .collect(),
            opt_out_effective_business_days: Some(15),
        }
    }
}
