//! Tax settings (09.6): the enable/disable toggle, the compliance
//! preflight, and the scheduled re-check.
//!
//! The preflight is the whole point. Stripe Tax collects nothing where
//! the liable account has no active registration and returns **no
//! error** while doing so — a silent failure that surfaces months later
//! as an unpaid liability. Enabling therefore requires, checked against
//! the provider right now:
//!
//! 1. the connected account's tax `status == active`, **and**
//! 2. at least one active registration.
//!
//! A failed preflight leaves the toggle off and names exactly which step
//! is missing (`tax_status_not_active` / `no_active_registration`).
//!
//! We never advise: registration and remittance are the tenant's legal
//! responsibility — every response carries that statement and points at
//! their tax advisor. This module never suggests jurisdictions.

use std::sync::Arc;

use chrono::Utc;
use serde::Serialize;
use uuid::Uuid;

use crate::domain::error::PaymentsError;
use crate::domain::ports::{
    ConnectionNotifier, ProviderConnectionRepository, TaxSettingsRepository,
};
use crate::domain::tax::TaxSettingsRecord;
use sanvi_persistence::TxManager;

/// The tenant-facing view: our toggle, the last observed snapshot, any
/// lapse warning — and the standing legal statement.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TaxSettingsView {
    pub enabled: bool,
    /// The liability routing statement, verbatim for the UI: with direct
    /// charges the connected account is liable; this is its id.
    pub liability_account: Option<String>,
    pub provider_status: String,
    pub active_registrations: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<TaxWarningView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_checked_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TaxWarningView {
    /// Stable i18n key (`payments.tax.warning.*`).
    pub code: String,
    pub detail: String,
    pub detected_at: chrono::DateTime<chrono::Utc>,
}

impl TaxSettingsView {
    fn of(record: &TaxSettingsRecord) -> Self {
        Self {
            enabled: record.enabled,
            liability_account: if record.liability_account.is_empty() {
                None
            } else {
                Some(record.liability_account.clone())
            },
            provider_status: record.provider_status.clone(),
            active_registrations: record.active_registrations,
            warning: record.warning.as_ref().map(|w| TaxWarningView {
                code: w.code.clone(),
                detail: w.detail.clone(),
                detected_at: w.detected_at,
            }),
            last_checked_at: record.last_checked_at,
        }
    }
}

const ACTOR: &str = "payments-tax-settings";

pub struct ManageTaxSettings {
    tx_manager: TxManager,
    connections: Arc<dyn ProviderConnectionRepository>,
    registry: Arc<crate::application::ProviderRegistry>,
    tax_settings: Arc<dyn TaxSettingsRepository>,
}

impl ManageTaxSettings {
    pub fn new(
        tx_manager: TxManager,
        connections: Arc<dyn ProviderConnectionRepository>,
        registry: Arc<crate::application::ProviderRegistry>,
        tax_settings: Arc<dyn TaxSettingsRepository>,
    ) -> Self {
        Self {
            tx_manager,
            connections,
            registry,
            tax_settings,
        }
    }

    async fn load_connection(
        &self,
        tenant_id: Uuid,
    ) -> Result<crate::domain::connection::ProviderConnection, PaymentsError> {
        let mut scope = self
            .tx_manager
            .begin_for(tenant_id)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        let connections = self.connections.list(&mut scope, tenant_id).await;
        let _ = scope.rollback().await;
        connections?
            .into_iter()
            .find(|c| c.disconnected_at.is_none())
            .ok_or_else(|| {
                PaymentsError::ConnectionNotActive(
                    "payments.blocker.onboarding_not_started".to_string(),
                )
            })
    }

    async fn persist(&self, record: &TaxSettingsRecord) -> Result<(), PaymentsError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("save_tax_settings", ACTOR)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        self.tax_settings.upsert(&mut scope, record).await?;
        scope
            .commit()
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        Ok(())
    }

    /// `GET /tenant/payments/tax-settings`.
    pub async fn view(&self, tenant_id: Uuid) -> Result<TaxSettingsView, PaymentsError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("load_tax_settings", ACTOR)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        let record = self
            .tax_settings
            .find_by_tenant(&mut scope, tenant_id)
            .await?;
        let _ = scope.rollback().await;
        Ok(TaxSettingsView::of(
            &record.unwrap_or_else(|| TaxSettingsRecord::disabled(tenant_id)),
        ))
    }

    /// `PUT /tenant/payments/tax-settings { enabled: true }` — run the
    /// preflight against the connected account *now*, refuse with the
    /// specific missing step on failure, store the snapshot + liability on
    /// success.
    pub async fn enable(&self, tenant_id: Uuid) -> Result<TaxSettingsView, PaymentsError> {
        let connection = self.load_connection(tenant_id).await?;
        let adapter = self.registry.get(connection.provider).ok_or_else(|| {
            PaymentsError::InvalidConnection(format!(
                "provider {:?} is not available in this environment",
                connection.provider.as_str()
            ))
        })?;

        // The preflight, live against the provider. Compliance fetched
        // under the account's own identity — registrations belong to the
        // liable entity.
        let snapshot = adapter
            .fetch_tax_compliance(&connection.external_account_id)
            .await?;

        // Load-or-create the record (RLS-scoped reads happen through the
        // platform-scope port here: the row may not exist yet).
        let mut scope = self
            .tx_manager
            .begin_unscoped("load_tax_settings_for_enable", ACTOR)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        let existing = self
            .tax_settings
            .find_by_tenant(&mut scope, tenant_id)
            .await?;
        let _ = scope.rollback().await;
        let mut record = existing.unwrap_or_else(|| TaxSettingsRecord::disabled(tenant_id));

        match snapshot.preflight() {
            Ok(()) => {
                record.enable(&connection.external_account_id, &snapshot);
                self.persist(&record).await?;
                Ok(TaxSettingsView::of(&record))
            }
            Err(rejection) => {
                // The toggle stays off; the API says exactly which step is
                // missing. Persist the observed snapshot anyway so the UI
                // can show current state next to the refusal.
                record.observe(&connection.external_account_id, &snapshot, Utc::now());
                self.persist(&record).await?;
                Err(PaymentsError::TaxPreflightFailed {
                    reason: rejection.code().to_string(),
                })
            }
        }
    }

    /// `PUT /tenant/payments/tax-settings { enabled: false }`. Always
    /// permitted. Disabling mid-period changes what customers are charged,
    /// so the response is a tenant-facing change requiring notice — the
    /// handler surfaces the disclosure copy, never a silent flag flip.
    pub async fn disable(&self, tenant_id: Uuid) -> Result<TaxSettingsView, PaymentsError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("load_tax_settings_for_disable", ACTOR)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        let existing = self
            .tax_settings
            .find_by_tenant(&mut scope, tenant_id)
            .await?;
        let _ = scope.rollback().await;
        let mut record = existing.unwrap_or_else(|| TaxSettingsRecord::disabled(tenant_id));
        record.disable();
        self.persist(&record).await?;
        Ok(TaxSettingsView::of(&record))
    }
}

/// The scheduled re-check (and its webhook-triggered variant): every
/// tenant with automatic tax enabled gets their compliance re-verified;
/// lapsed registrations warn instead of silently under-collecting.
pub struct CheckTenantTaxCompliance {
    tx_manager: TxManager,
    connections: Arc<dyn ProviderConnectionRepository>,
    registry: Arc<crate::application::ProviderRegistry>,
    tax_settings: Arc<dyn TaxSettingsRepository>,
    notifier: Arc<dyn ConnectionNotifier>,
    metrics: Option<crate::adapters::PaymentsMetrics>,
}

impl CheckTenantTaxCompliance {
    pub fn new(
        tx_manager: TxManager,
        connections: Arc<dyn ProviderConnectionRepository>,
        registry: Arc<crate::application::ProviderRegistry>,
        tax_settings: Arc<dyn TaxSettingsRepository>,
        notifier: Arc<dyn ConnectionNotifier>,
    ) -> Self {
        Self {
            tx_manager,
            connections,
            registry,
            tax_settings,
            notifier,
            metrics: None,
        }
    }

    pub fn with_metrics(mut self, metrics: crate::adapters::PaymentsMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Re-verify up to `limit` enabled tenants, oldest check first.
    pub async fn execute(&self, limit: i64) -> Result<ComplianceReport, PaymentsError> {
        let mut report = ComplianceReport::default();
        let batch = {
            let mut scope = self
                .tx_manager
                .begin_unscoped("list_tax_enabled_tenants", ACTOR)
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            let batch = self.tax_settings.list_enabled(&mut scope, limit).await?;
            scope
                .commit()
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            batch
        };

        for record in batch {
            // Resolve the account to check. A stored liability account is
            // authoritative enough for a re-check; a blank one means the
            // row predates the account capture and needs a connection
            // lookup instead.
            let account = if record.liability_account.is_empty() {
                match self.load_connection_account(record.tenant_id).await {
                    Some(account) => account,
                    None => {
                        report.skipped += 1;
                        continue;
                    }
                }
            } else {
                record.liability_account.clone()
            };
            let provider = self.provider_for(record.tenant_id).await;
            let Some(adapter) = provider else {
                report.skipped += 1;
                continue;
            };
            match adapter.fetch_tax_compliance(&account).await {
                Ok(snapshot) => {
                    let mut updated = record.clone();
                    updated.observe(&account, &snapshot, Utc::now());
                    let lapsed = updated.warning.is_some();
                    self.persist(&updated).await?;
                    report.checked += 1;
                    if lapsed {
                        report.warned += 1;
                        // The warning path: alert the tenant — their money
                        // (and liability) is accumulating unrecorded.
                        if let Err(err) = self
                            .notifier
                            .tax_compliance_lapsed(record.tenant_id, &account)
                            .await
                        {
                            tracing::warn!(
                                tenant_id = %record.tenant_id,
                                error = %err,
                                "tax-compliance lapse notification failed"
                            );
                        }
                    }
                    if let Some(metrics) = &self.metrics
                        && lapsed
                    {
                        metrics.tax_compliance_lapsed.inc();
                    }
                }
                Err(err) => {
                    report.errors += 1;
                    tracing::error!(
                        tenant_id = %record.tenant_id,
                        account = %account,
                        error = %err,
                        "tax compliance re-check failed"
                    );
                }
            }
        }
        Ok(report)
    }

    /// One tenant's re-check, callable from webhook handlers when a
    /// tax-related account event arrives.
    pub async fn recheck_tenant(&self, tenant_id: Uuid) -> Result<(), PaymentsError> {
        let Some(adapter) = self.provider_for(tenant_id).await else {
            return Ok(());
        };
        let Some(account) = self.load_connection_account(tenant_id).await else {
            return Ok(());
        };
        let mut scope = self
            .tx_manager
            .begin_unscoped("load_tax_settings_for_recheck", ACTOR)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        let existing = self
            .tax_settings
            .find_by_tenant(&mut scope, tenant_id)
            .await?
            .filter(|settings| settings.enabled);
        let _ = scope.rollback().await;
        let Some(mut record) = existing else {
            return Ok(());
        };

        let snapshot = adapter.fetch_tax_compliance(&account).await?;
        record.observe(&account, &snapshot, Utc::now());
        let lapsed = record.warning.is_some();
        self.persist(&record).await?;
        if lapsed {
            if let Err(err) = self
                .notifier
                .tax_compliance_lapsed(tenant_id, &account)
                .await
            {
                tracing::warn!(tenant_id = %tenant_id, error = %err, "tax-compliance lapse notification failed");
            }
        }
        Ok(())
    }

    async fn provider_for(
        &self,
        tenant_id: Uuid,
    ) -> Option<Arc<dyn crate::domain::PaymentProviderPort>> {
        self.registry
            .get(self.load_connection_provider(tenant_id).await?)
    }

    async fn load_connection_provider(
        &self,
        tenant_id: Uuid,
    ) -> Option<crate::domain::ProviderKind> {
        let mut scope = self.tx_manager.begin_for(tenant_id).await.ok()?;
        let list = self.connections.list(&mut scope, tenant_id).await.ok()?;
        let _ = scope.rollback().await;
        list.into_iter()
            .find(|c| c.disconnected_at.is_none())
            .map(|c| c.provider)
    }

    async fn load_connection_account(&self, tenant_id: Uuid) -> Option<String> {
        let mut scope = self.tx_manager.begin_for(tenant_id).await.ok()?;
        let list = self.connections.list(&mut scope, tenant_id).await.ok()?;
        let _ = scope.rollback().await;
        list.into_iter()
            .find(|c| c.disconnected_at.is_none())
            .map(|c| c.external_account_id)
    }

    async fn persist(&self, record: &TaxSettingsRecord) -> Result<(), PaymentsError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("save_tax_settings_recheck", ACTOR)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        self.tax_settings.upsert(&mut scope, record).await?;
        scope
            .commit()
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        Ok(())
    }
}

/// One re-check pass's outcome.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct ComplianceReport {
    pub checked: u32,
    /// Tenants whose compliance has lapsed — each warned.
    pub warned: u32,
    /// Provider/adapter failures — retried on the next pass.
    pub errors: u32,
    /// Rows that could not be resolved to a live connection/provider.
    pub skipped: u32,
}
