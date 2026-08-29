//! `sanvi-worker` — background worker: outbox dispatcher, stream consumers
//! and the leader-elected scheduler.
//!
//! - Outbox dispatcher relays unpublished domain events to per-context
//!   Redis Streams, dead-lettering rows that exhaust retries.
//! - Consumers process `sanvi.events.tenancy` (cache invalidation),
//!   `sanvi.events.identity` (Keto sync) and `sanvi.events.access`
//!   (entitlement cache invalidation, role → Keto tuple sync) with
//!   exactly-once semantics via `platform.processed_events`.
//! - The scheduler runs periodic jobs (purge sweep, idempotency sweep,
//!   quota rollover, entitlement expiry sweep, audit chain verification)
//!   on the elected leader only.
//! - Graceful shutdown: SIGTERM/SIGINT stops polling, in-flight batches
//!   complete, pools close.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use sanvi_access::{
    AccessConfig,
    adapters::{
        HttpKetoClient, InMemoryDecisionCache, OutboxEventPublisher as AccessEventPublisher,
        RedisEntitlementCache, SqlxEntitlementRepository, SqlxQuotaUsageStore, SqlxRoleRepository,
        sync::entitlement_sync_handler,
    },
    application::ExpireEntitlements,
    domain::{
        Clock, EntitlementCache, EntitlementRepository, EventPublisher, PermissionStore,
        QuotaUsageStore, RoleRepository,
    },
};
use sanvi_advertising::{
    ConversionSinkPort, PlatformKey,
    adapters::{
        AdvertisingMetrics, FakeAdPlatform, FakeConversionSink,
        OutboxEventPublisher as AdvertisingEventPublisher, SqlxAdvertisingConnectionRepository,
        SqlxCampaignRepository,
    },
    application::{
        ReconcileCampaign, ReconcileDueCampaigns, RetractWithdrawnConversions,
        UploadPendingConversions,
    },
    domain::AdvertisingConnectionRepository,
    domain::CampaignRepository as AdvertisingCampaignRepository,
    domain::ConversionEventRepository,
    domain::EventPublisher as AdvertisingEvents,
};
use sanvi_audit::{
    AuditConfig,
    adapters::{SqlxAuditReader, SqlxAuditWriter},
    application::VerifyAuditChain,
};
use sanvi_billing::{
    BillingConfig,
    adapters::{
        AccessEntitlementSyncer, HttpStripeClient, OutboxEventPublisher as BillingEventPublisher,
        SqlxInvoiceRepository, SqlxPlanEntitlementRepository, SqlxPlanPriceRepository,
        SqlxPlanRepository, SqlxSubscriptionRepository, SqlxWebhookEventRepository, StripeMetrics,
        SystemClock, SystemIdGenerator, TenancyTenantLifecycle, metrics::BillingMetrics,
    },
    application::{CheckTaxRegistrations, DunningSweep, ReconcileSubscriptions, WebhookDispatcher},
    domain::{
        Clock as BillingClock, EntitlementSyncer, EventPublisher as BillingEvents,
        IdGenerator as BillingIds, InvoiceRepository, Notifications, PlanEntitlementRepository,
        PlanPriceRepository, PlanRepository, StripeClient, SubscriptionRepository,
        WebhookEventRepository,
    },
};
use sanvi_domains::{DomainsConfig, adapters::events::domains_event_handler};
use sanvi_kernel::{IdsConfig, RawIdGenerator, SnowflakeGenerator, TenantId};
use sanvi_localization::{
    LocalizationConfig,
    adapters::{
        LocalizationCache, LocalizationMetrics, LocalizationService, NotificationLocalizer,
        SqlxTranslationRepository, events::localization_event_handler,
    },
    domain::Arg as LocalizationArg,
};
use sanvi_messaging::{
    ConsumerMetrics, FnJob, OutboxDispatcher, OutboxMetrics, RedisConfig, Scheduler,
    StreamConsumer, StreamName, StreamsClient, connect_redis,
};
use sanvi_payments::{
    adapters::{
        OutboxEventPublisher as PaymentsEvents, SqlxProviderConnectionRepository,
        SqlxWebhookEventRepository as PaymentsWebhookEvents,
    },
    application::{
        ProviderRegistry as PaymentsProviderRegistry,
        RuntimeEnvironment as PaymentsRuntimeEnvironment, SyncConnections,
        WebhookDispatcher as ConnectWebhookDispatcher,
    },
    domain::{
        ConnectionLifecycleStore, ConnectionNotifier as PaymentsConnectionNotifier,
        EventPublisher as PaymentsEventPublisher,
        WebhookEventRepository as PaymentsWebhookEventRepository,
    },
};
use sanvi_persistence::{DatabaseConfig, build_pool};
use sanvi_privacy::{
    PrivacyConfig,
    adapters::{
        DisabledEnvelopeCrypto, LoggingSubProcessorNotifier, OutboxEventPublisher as PrivacyEvents,
        PrivacyContextProvider, Sha256Hasher, SqlxAppealRepository, SqlxAuthorizedAgentRepository,
        SqlxBreachRepository, SqlxConsentRepository, SqlxDirectiveRepository, SqlxDsrRepository,
        SqlxEnvelopeCrypto, SqlxExportArtifactRepository, SqlxJurisdictionRepository,
        SqlxLegalHoldRepository, SqlxParticipantRepository, SqlxRequestMetricsRepository,
        SqlxRetentionRuleRepository, SqlxSignalRepository, SqlxSubProcessorRepository,
        SqlxSubjectKeyRepository, SqlxSubjectRepository, SqlxVerificationRepository,
        SystemClock as PrivacySystemClock, SystemIdGenerator as PrivacySystemIdGenerator,
        ZipPackager,
    },
    application::{
        CleanupExpiredExports, GenerateAnnualMetrics, PersistedDirectiveResolver, ProcessDsrBatch,
        PropagateOptOuts, RetentionSweep,
    },
    domain::{
        AppealRepository, AuthorizedAgentRepository, BreachRepository, ConsentRepository,
        DirectiveRepository, DirectiveResolver, DsrRepository, EnvelopeCrypto,
        ExportArtifactRepository, Hasher, JurisdictionRepository, LegalHoldRepository,
        Notifications as PrivacyNotificationsTrait, ParticipantRepository, ProviderRegistry,
        RequestMetricsRepository, RetentionRuleRepository, SignalRepository, SubProcessorNotifier,
        SubProcessorRepository, SubjectKeyRepository, SubjectRepository, VerificationRepository,
    },
};
use sanvi_telemetry::TelemetryConfig;
use sanvi_tenancy::{
    TenancyConfig,
    adapters::{
        OutboxEventPublisher as TenancyEventPublisher, SystemClock as TenancySystemClock,
        events::tenant_event_handler, repo::SqlxTenantRepository,
        resolution::RedisTenantResolutionCache,
    },
    application::{PurgeSweep, ResolveTenant},
};
use sanvi_theming::{
    ThemingConfig,
    adapters::{ResolvedThemeCache, events::theming_event_handler},
};
use serde::{Deserialize, Serialize};

struct WorkerBillingNotifications {
    localizer: Arc<NotificationLocalizer>,
}

impl WorkerBillingNotifications {
    async fn render(
        &self,
        tenant_id: TenantId,
        subject_key: &str,
        body_key: &str,
        extra_args: &[(&str, LocalizationArg)],
    ) {
        let context = self.localizer.tenant_locale(tenant_id).await;
        let tenant = tenant_id.to_string();
        let tenant_arg = [("tenant", LocalizationArg::from(tenant))];
        let subject = self.localizer.render(&context, subject_key, &tenant_arg);
        let mut body_args = tenant_arg.to_vec();
        body_args.extend_from_slice(extra_args);
        let body = self.localizer.render(&context, body_key, &body_args);
        tracing::info!(tenant_id = %tenant_id, locale = %context.locale, subject, body, "localized billing notification rendered");
    }
}

#[async_trait::async_trait]
impl Notifications for WorkerBillingNotifications {
    async fn payment_failed(
        &self,
        tenant_id: TenantId,
    ) -> Result<(), sanvi_billing::domain::BillingError> {
        self.render(
            tenant_id,
            "emails.payment_failed.subject",
            "emails.payment_failed.body",
            &[],
        )
        .await;
        Ok(())
    }

    async fn grace_expired(
        &self,
        tenant_id: TenantId,
    ) -> Result<(), sanvi_billing::domain::BillingError> {
        self.render(
            tenant_id,
            "emails.grace_expired.subject",
            "emails.grace_expired.body",
            &[],
        )
        .await;
        Ok(())
    }

    async fn trial_ending(
        &self,
        tenant_id: TenantId,
        trial_end: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), sanvi_billing::domain::BillingError> {
        self.render(
            tenant_id,
            "emails.trial_ending.subject",
            "emails.trial_ending.body",
            &[("date", LocalizationArg::from(trial_end))],
        )
        .await;
        Ok(())
    }

    async fn subscription_canceled(
        &self,
        tenant_id: TenantId,
    ) -> Result<(), sanvi_billing::domain::BillingError> {
        self.render(
            tenant_id,
            "emails.subscription_canceled.subject",
            "emails.subscription_canceled.body",
            &[],
        )
        .await;
        Ok(())
    }

    async fn subscription_restored(
        &self,
        tenant_id: TenantId,
    ) -> Result<(), sanvi_billing::domain::BillingError> {
        self.render(
            tenant_id,
            "emails.subscription_restored.subject",
            "emails.subscription_restored.body",
            &[],
        )
        .await;
        Ok(())
    }
}

/// Phase-09: tenant-admin notifications for connection-lifecycle changes,
/// rendered through the phase-06 localizer (tenant locale, override-aware
/// catalogs) — the same path billing's notifications take.
struct WorkerPaymentsNotifications {
    localizer: Arc<NotificationLocalizer>,
}

impl WorkerPaymentsNotifications {
    async fn render(
        &self,
        tenant_id: TenantId,
        subject_key: &str,
        body_key: &str,
        extra_args: &[(&str, LocalizationArg)],
    ) {
        let context = self.localizer.tenant_locale(tenant_id).await;
        let tenant = tenant_id.to_string();
        let tenant_arg = [("tenant", LocalizationArg::from(tenant))];
        let subject = self.localizer.render(&context, subject_key, &tenant_arg);
        let mut body_args = tenant_arg.to_vec();
        body_args.extend_from_slice(extra_args);
        let body = self.localizer.render(&context, body_key, &body_args);
        tracing::info!(tenant_id = %tenant_id, locale = %context.locale, subject, body, "localized payments notification rendered");
    }
}

#[async_trait::async_trait]
impl PaymentsConnectionNotifier for WorkerPaymentsNotifications {
    async fn connection_activated(
        &self,
        tenant_id: TenantId,
        connection_id: sanvi_payments::domain::ConnectionId,
    ) -> Result<(), sanvi_payments::domain::PaymentsError> {
        self.render(
            tenant_id,
            "emails.payments_connection_activated.subject",
            "emails.payments_connection_activated.body",
            &[(
                "connection",
                LocalizationArg::from(connection_id.to_string()),
            )],
        )
        .await;
        Ok(())
    }

    async fn connection_restricted(
        &self,
        tenant_id: TenantId,
        connection_id: sanvi_payments::domain::ConnectionId,
        detail: &str,
    ) -> Result<(), sanvi_payments::domain::PaymentsError> {
        self.render(
            tenant_id,
            "emails.payments_connection_restricted.subject",
            "emails.payments_connection_restricted.body",
            &[
                (
                    "connection",
                    LocalizationArg::from(connection_id.to_string()),
                ),
                ("detail", LocalizationArg::from(detail)),
            ],
        )
        .await;
        Ok(())
    }

    async fn requirement_deadline_approaching(
        &self,
        tenant_id: TenantId,
        connection_id: sanvi_payments::domain::ConnectionId,
        deadline: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), sanvi_payments::domain::PaymentsError> {
        self.render(
            tenant_id,
            "emails.payments_requirement_deadline.subject",
            "emails.payments_requirement_deadline.body",
            &[
                (
                    "connection",
                    LocalizationArg::from(connection_id.to_string()),
                ),
                ("date", LocalizationArg::from(deadline)),
            ],
        )
        .await;
        Ok(())
    }

    async fn dispute_opened(
        &self,
        tenant_id: TenantId,
        dispute_id: sanvi_payments::domain::DisputeId,
        due_by: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<(), sanvi_payments::domain::PaymentsError> {
        let dispute_arg = [("dispute", LocalizationArg::from(dispute_id.to_string()))];
        match due_by {
            Some(due) => {
                let mut args = dispute_arg.to_vec();
                args.push(("due", LocalizationArg::from(due)));
                self.render(
                    tenant_id,
                    "emails.payments_dispute_opened.subject",
                    "emails.payments_dispute_opened.body_due",
                    &args,
                )
                .await;
            }
            None => {
                self.render(
                    tenant_id,
                    "emails.payments_dispute_opened.subject",
                    "emails.payments_dispute_opened.body",
                    &dispute_arg,
                )
                .await;
            }
        }
        Ok(())
    }

    async fn payout_failed(
        &self,
        tenant_id: TenantId,
        external_payout_id: &str,
        amount: &sanvi_payments::Money,
    ) -> Result<(), sanvi_payments::domain::PaymentsError> {
        // A failed payout means the tenant's money is stuck at the
        // provider — an alert, not a row.
        self.render(
            tenant_id,
            "emails.payments_payout_failed.subject",
            "emails.payments_payout_failed.body",
            &[
                (
                    "payout",
                    LocalizationArg::from(external_payout_id.to_string()),
                ),
                (
                    "amount",
                    LocalizationArg::from(format!(
                        "{} {}",
                        amount.amount_minor,
                        amount.currency.as_str()
                    )),
                ),
            ],
        )
        .await;
        tracing::error!(
            tenant_id = %tenant_id,
            payout = %external_payout_id,
            "tenant notified of a FAILED payout"
        );
        Ok(())
    }

    async fn tax_compliance_lapsed(
        &self,
        tenant_id: TenantId,
        detail: &str,
    ) -> Result<(), sanvi_payments::domain::PaymentsError> {
        self.render(
            tenant_id,
            "emails.payments_tax_lapsed.subject",
            "emails.payments_tax_lapsed.body",
            &[("detail", LocalizationArg::from(detail.to_string()))],
        )
        .await;
        Ok(())
    }
}

struct WorkerPrivacyNotifications {
    localizer: Arc<NotificationLocalizer>,
}

#[async_trait::async_trait]
impl PrivacyNotificationsTrait for WorkerPrivacyNotifications {
    async fn send_verification_code(
        &self,
        to: &str,
        code: &str,
    ) -> Result<(), sanvi_privacy::domain::PrivacyError> {
        let context = self.localizer.platform_locale().await;
        let subject = self
            .localizer
            .render(&context, "emails.verification.subject", &[]);
        let body = self.localizer.render(
            &context,
            "emails.verification.body",
            &[("code", LocalizationArg::from(code))],
        );
        tracing::info!(to, locale = %context.locale, subject, body, "localized privacy notification rendered");
        Ok(())
    }

    async fn send_export_credentials(
        &self,
        subject_ref: &sanvi_privacy::domain::SubjectRef,
        token: &str,
        passphrase: &str,
        request_id: &str,
    ) -> Result<(), sanvi_privacy::domain::PrivacyError> {
        let context = match subject_ref.tenant_id {
            Some(tenant_id) => self.localizer.tenant_locale(tenant_id).await,
            None => self.localizer.platform_locale().await,
        };
        let subject = self
            .localizer
            .render(&context, "emails.export_passphrase.subject", &[]);
        let body = self.localizer.render(
            &context,
            "emails.export_passphrase.body",
            &[
                ("request_id", LocalizationArg::from(request_id)),
                ("passphrase", LocalizationArg::from(passphrase)),
            ],
        );
        tracing::info!(subject_ref = %subject_ref.canonical_key(), token, locale = %context.locale, subject, body, "localized privacy export notification rendered");
        Ok(())
    }

    async fn send_request_status(
        &self,
        to: &str,
        request_subject: &str,
        status: &str,
    ) -> Result<(), sanvi_privacy::domain::PrivacyError> {
        let context = self.localizer.platform_locale().await;
        let subject = self.localizer.render(
            &context,
            "emails.privacy_status.subject",
            &[("subject", LocalizationArg::from(request_subject))],
        );
        let body = self.localizer.render(
            &context,
            "emails.privacy_status.body",
            &[("status", LocalizationArg::from(status))],
        );
        tracing::info!(to, locale = %context.locale, subject, body, "localized privacy status notification rendered");
        Ok(())
    }
}

/// Outbox dispatcher tuning.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct OutboxConfig {
    pub poll_interval_ms: u64,
    pub batch_size: usize,
    pub max_attempts: i32,
    pub stream: String,
}

impl Default for OutboxConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 1000,
            batch_size: 100,
            max_attempts: 5,
            stream: "sanvi.events".to_string(),
        }
    }
}

/// Scheduler tuning.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SchedulerConfig {
    /// Interval of the purge sweep (archived tenants → purge requests).
    pub purge_sweep_interval_secs: u64,
    /// Retention before an archived tenant gets a purge request (days).
    pub purge_retention_days: u32,
    /// Idempotency-key sweep interval.
    pub idempotency_sweep_interval_secs: u64,
    /// Quota counter period rollover (drops usage rows older than the
    /// current period).
    pub quota_rollover_interval_secs: u64,
    /// Entitlement expiry sweep (deletes expired overrides + events).
    pub entitlement_expiry_sweep_interval_secs: u64,
    /// Audit hash-chain verification.
    pub audit_verify_interval_secs: u64,
    /// Stripe webhook event dispatch (phase 04).
    pub webhook_dispatch_interval_secs: u64,
    /// Dunning grace sweep (phase 04).
    pub dunning_sweep_interval_secs: u64,
    /// Stripe → local reconciliation (phase 04).
    pub reconciliation_interval_secs: u64,
    /// Stripe Tax registration check (phase 04).
    pub tax_check_interval_secs: u64,
    /// Billing gauges refresh (phase 04).
    pub billing_metrics_interval_secs: u64,
    /// DSR saga advancement pass (phase 05).
    pub dsr_advance_interval_secs: u64,
    /// Opt-out propagation to downstream processors (phase 05).
    pub optout_propagation_interval_secs: u64,
    /// Retention sweep (dry-run by default; phase 05).
    pub retention_sweep_interval_secs: u64,
    /// Expired export cleanup (phase 05).
    pub export_cleanup_interval_secs: u64,
    /// Annual request-metrics generation (phase 05).
    pub privacy_metrics_interval_secs: u64,
    /// DNS verification polling (phase 08).
    pub domains_verification_interval_secs: u64,
    /// Certificate issuance sweep (phase 08).
    pub domains_cert_issue_interval_secs: u64,
    /// Certificate/order renewal sweep (phase 08).
    pub domains_renewal_interval_secs: u64,
    /// Live-domain health checks (phase 08).
    pub domains_health_interval_secs: u64,
    /// Purchase saga advancement + refund retries (phase 08).
    pub domains_orders_interval_secs: u64,
    /// Connect webhook event dispatch (phase 09).
    pub payments_webhook_dispatch_interval_secs: u64,
    /// Connection status reconciliation (phase 09) — the safety net for
    /// missed webhook deliveries.
    pub payments_connection_sync_interval_secs: u64,
    /// Payments gauges refresh (phase 09).
    pub payments_metrics_interval_secs: u64,
    /// Tax-compliance re-check for tenants with automatic tax enabled
    /// (phase 09.6) — lapsed registrations warn instead of silently
    /// under-collecting.
    pub payments_tax_check_interval_secs: u64,
    /// Pre-create current and following advertising partitions. This starts
    /// before any conversion endpoint exists: partition maintenance cannot be
    /// an afterthought on the first day of a new month.
    pub advertising_partition_maintenance_interval_secs: u64,
    /// Cross-tenant campaign reconciliation sweep (TASK-012, 10.3) — the
    /// nightly safety net alongside on-demand reconciliation. Never mutates
    /// a platform; only observes and records drift.
    pub advertising_reconciliation_interval_secs: u64,
    /// Conversion upload worker sweep (TASK-015, 10.6).
    pub advertising_conversion_upload_interval_secs: u64,
    /// Conversion retraction worker sweep (TASK-015, 10.6).
    pub advertising_conversion_retraction_interval_secs: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            purge_sweep_interval_secs: 3600,
            purge_retention_days: 30,
            idempotency_sweep_interval_secs: 3600,
            quota_rollover_interval_secs: 86400,
            entitlement_expiry_sweep_interval_secs: 3600,
            audit_verify_interval_secs: 86400,
            webhook_dispatch_interval_secs: 10,
            dunning_sweep_interval_secs: 3600,
            reconciliation_interval_secs: 86400,
            tax_check_interval_secs: 86400,
            billing_metrics_interval_secs: 60,
            dsr_advance_interval_secs: 30,
            optout_propagation_interval_secs: 60,
            retention_sweep_interval_secs: 86400,
            export_cleanup_interval_secs: 3600,
            privacy_metrics_interval_secs: 86400,
            domains_verification_interval_secs: 60,
            domains_cert_issue_interval_secs: 30,
            domains_renewal_interval_secs: 3600,
            domains_health_interval_secs: 3600,
            domains_orders_interval_secs: 60,
            payments_webhook_dispatch_interval_secs: 10,
            payments_connection_sync_interval_secs: 3600,
            payments_metrics_interval_secs: 60,
            payments_tax_check_interval_secs: 86_400,
            advertising_partition_maintenance_interval_secs: 86_400,
            advertising_reconciliation_interval_secs: 86_400,
            advertising_conversion_upload_interval_secs: 60,
            advertising_conversion_retraction_interval_secs: 60,
        }
    }
}

/// Ory endpoints the worker needs: Keto for the access sync consumer,
/// Kratos for the identity privacy provider's erasure calls. Same
/// `[ory]` section the api reads, so the two never drift.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct OryConfig {
    pub kratos_public_url: String,
    pub kratos_admin_url: String,
    pub keto_read_url: String,
    pub keto_write_url: String,
}

impl Default for OryConfig {
    fn default() -> Self {
        Self {
            kratos_public_url: "http://localhost:4433".to_string(),
            kratos_admin_url: "http://localhost:4434".to_string(),
            keto_read_url: "http://localhost:4466".to_string(),
            keto_write_url: "http://localhost:4467".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct WorkerConfig {
    #[serde(default)]
    pub ids: IdsConfig,
    pub database: DatabaseConfig,
    pub redis: RedisConfig,
    pub telemetry: TelemetryConfig,
    pub outbox: OutboxConfig,
    pub scheduler: SchedulerConfig,
    pub tenancy: TenancyConfig,
    #[serde(default)]
    pub ory: OryConfig,
    /// Read for `session_cookie_name`: the identity privacy provider talks
    /// to Kratos, and a deployment that renames the cookie renames it for
    /// every process, not just the api.
    #[serde(default)]
    pub identity: sanvi_identity::IdentityConfig,
    pub access: AccessConfig,
    pub audit: AuditConfig,
    pub billing: BillingConfig,
    pub privacy: PrivacyConfig,
    pub localization: LocalizationConfig,
    #[serde(default)]
    pub theming: ThemingConfig,
    #[serde(default)]
    pub domains: DomainsConfig,
    /// Phase 09: payments configuration (per-environment secrets and the
    /// application-fee switch). No worker job consumes it until 09.3's
    /// webhook dispatcher and sync job land; declaring it now keeps the
    /// api and worker reading the same sections.
    #[serde(default)]
    pub payments: sanvi_payments::PaymentsConfig,
    #[serde(default)]
    pub advertising: sanvi_advertising::AdvertisingConfig,
}

impl WorkerConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.database.validate()?;
        self.redis.validate()?;
        self.telemetry.validate()?;
        self.tenancy.validate()?;
        self.identity.validate()?;
        self.access.validate()?;
        self.audit.validate()?;
        self.billing.validate()?;
        self.privacy.validate()?;
        self.localization.validate()?;
        self.theming.validate()?;
        self.domains.validate()?;
        self.payments.validate()?;
        self.advertising.validate()?;
        if self.outbox.batch_size == 0 {
            anyhow::bail!("outbox.batch_size must be >= 1");
        }
        if self.scheduler.purge_retention_days == 0 {
            anyhow::bail!("scheduler.purge_retention_days must be >= 1");
        }
        Ok(())
    }
}

fn load_config() -> anyhow::Result<WorkerConfig> {
    let environment = std::env::var("SANVI_ENV").unwrap_or_else(|_| "local".to_string());
    let config: WorkerConfig = Figment::new()
        .merge(Toml::file("configs/base.toml"))
        .merge(Toml::file(format!("configs/{environment}.toml")))
        .merge(Env::prefixed("SANVI__").split("__"))
        .extract()
        .context("failed to load configuration")?;
    config.validate()?;
    config.ids.resolve(&config.telemetry.environment, 2)?;
    Ok(config)
}

#[tokio::main]
async fn main() -> ExitCode {
    let config = match load_config() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("configuration error:\n{err:#}");
            return ExitCode::from(2);
        }
    };

    let node_id = match config.ids.resolve(&config.telemetry.environment, 2) {
        Ok(node_id) => node_id,
        Err(err) => {
            eprintln!("configuration error:\n{err:#}");
            return ExitCode::from(2);
        }
    };
    let ids: Arc<dyn RawIdGenerator> = Arc::new(SnowflakeGenerator::new(node_id));

    let (metrics, mut telemetry_guard) = match sanvi_telemetry::init(config.telemetry.clone()) {
        Ok(guard) => guard,
        Err(err) => {
            eprintln!("failed to initialise telemetry: {err:#}");
            return ExitCode::from(2);
        }
    };

    let pool = match build_pool(&config.database).await {
        Ok(pool) => pool,
        Err(err) => {
            eprintln!("failed to connect to postgres: {err:#}");
            return ExitCode::from(1);
        }
    };

    let redis = match connect_redis(&config.redis).await {
        Ok(manager) => manager,
        Err(err) => {
            eprintln!("failed to connect to redis: {err:#}");
            return ExitCode::from(1);
        }
    };

    let registry = metrics.prometheus_registry();
    let outbox_metrics = Arc::new(OutboxMetrics::new());
    if let Err(err) = outbox_metrics.register(registry) {
        eprintln!("failed to register outbox metrics: {err}");
        return ExitCode::from(1);
    }

    let streams = StreamsClient::new(redis.clone());

    let dispatcher = OutboxDispatcher::new(
        pool.clone(),
        streams.clone(),
        StreamName(config.outbox.stream.clone()),
        config.outbox.batch_size,
        config.outbox.max_attempts,
    )
    .with_metrics(outbox_metrics.clone());

    // --- tenancy consumer: keeps resolution caches coherent -----------------
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let tenants: Arc<dyn sanvi_tenancy::domain::TenantRepository> =
        Arc::new(SqlxTenantRepository::new());
    let resolution_cache = Arc::new(RedisTenantResolutionCache::new(
        redis.clone(),
        Duration::from_secs(config.tenancy.resolution_cache_ttl_secs),
    ));
    let resolve = Arc::new(ResolveTenant::new(
        tx_manager.clone(),
        tenants.clone(),
        resolution_cache.clone(),
    ));
    let invalidate = {
        let resolve = resolve.clone();
        move |slug: String, tenant_id: TenantId| {
            let resolve = resolve.clone();
            tokio::spawn(async move {
                resolve.invalidate(&slug, tenant_id).await;
            });
        }
    };
    let tenancy_consumer = StreamConsumer::new(pool.clone(), streams.clone());
    let tenancy_consumer_metrics = Arc::new(ConsumerMetrics::new("tenancy"));
    if let Err(err) = tenancy_consumer_metrics.register(registry) {
        eprintln!("failed to register tenancy consumer metrics: {err}");
        return ExitCode::from(1);
    }
    let tenancy_consumer = tenancy_consumer.with_metrics(tenancy_consumer_metrics);
    let tenancy_consumer_name = format!("tenancy-{}", uuid::Uuid::now_v7());

    // --- localization consumer: keeps merged-catalog caches coherent -------
    // Worker-rendered output (notifications, exports) must never serve stale
    // copy after another process edits overrides or tenant locale settings.
    let localization_service = Arc::new(LocalizationService::new(
        pool.clone(),
        ids.clone(),
        Arc::new(SqlxTranslationRepository::new()),
        Arc::new(LocalizationCache::new()),
        Arc::new(LocalizationMetrics::new(registry)),
        config.localization.clone(),
    ));
    let notification_localizer = Arc::new(NotificationLocalizer::new(
        pool.clone(),
        ids.clone(),
        localization_service.clone(),
        config.localization.clone(),
    ));
    let localization_consumer = StreamConsumer::new(pool.clone(), streams.clone());
    let localization_consumer_metrics = Arc::new(ConsumerMetrics::new("localization"));
    if let Err(err) = localization_consumer_metrics.register(registry) {
        eprintln!("failed to register localization consumer metrics: {err}");
        return ExitCode::from(1);
    }
    let localization_consumer = localization_consumer.with_metrics(localization_consumer_metrics);
    let localization_consumer_name = format!("localization-{}", uuid::Uuid::now_v7());

    // --- theming consumer: cross-instance resolved-theme invalidation ------
    // Publish/rollback/asset events clear every instance's cached resolved
    // theme so a tenant's new look is visible within one request.
    let theming_cache = Arc::new(ResolvedThemeCache::new(Some(redis.clone())));
    let theming_consumer = StreamConsumer::new(pool.clone(), streams.clone());
    let theming_consumer_metrics = Arc::new(ConsumerMetrics::new("theming"));
    if let Err(err) = theming_consumer_metrics.register(registry) {
        eprintln!("failed to register theming consumer metrics: {err}");
        return ExitCode::from(1);
    }
    let theming_consumer = theming_consumer.with_metrics(theming_consumer_metrics);
    let theming_consumer_name = format!("theming-{}", uuid::Uuid::now_v7());

    // --- domains consumer: host→tenant resolution-cache invalidation ------
    // Verification/live/removal events clear the shared + local host cache
    // so the tenancy resolver never serves a stale custom-domain mapping.
    let domains_consumer = StreamConsumer::new(pool.clone(), streams.clone());
    let domains_consumer_metrics = Arc::new(ConsumerMetrics::new("domains"));
    if let Err(err) = domains_consumer_metrics.register(registry) {
        eprintln!("failed to register domains consumer metrics: {err}");
        return ExitCode::from(1);
    }
    let domains_consumer = domains_consumer.with_metrics(domains_consumer_metrics);
    let domains_consumer_name = format!("domains-{}", uuid::Uuid::now_v7());

    // --- access consumer: keeps Keto in sync with identity events -----------
    let access_store: Arc<dyn PermissionStore> = match HttpKetoClient::new(
        config.ory.keto_read_url.clone(),
        config.ory.keto_write_url.clone(),
    ) {
        Ok(client) => Arc::new(client),
        Err(err) => {
            eprintln!("failed to build the keto client: {err}");
            return ExitCode::from(2);
        }
    };
    let access_roles: Arc<dyn RoleRepository> = Arc::new(SqlxRoleRepository::new());
    let access_tx = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let role_key_resolver = {
        let roles = access_roles.clone();
        let tx = access_tx.clone();
        move |role_id: sanvi_access::domain::RoleId| {
            let roles = roles.clone();
            let tx = tx.clone();
            Box::pin(async move {
                let mut scope = match tx.begin_unscoped("access_role_key_lookup", "access").await {
                    Ok(scope) => scope,
                    Err(err) => {
                        tracing::error!(error = %err, "role key lookup failed");
                        return None;
                    }
                };
                let key = match roles.find_by_id(&mut scope, role_id).await {
                    Ok(Some(role)) => Some(role.key().to_string()),
                    Ok(None) => None,
                    Err(err) => {
                        tracing::error!(error = %err, "role key lookup failed");
                        None
                    }
                };
                let _ = scope.commit().await;
                key
            })
                as std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
        }
    };
    let access_consumer = StreamConsumer::new(pool.clone(), streams.clone());
    let access_consumer_metrics = Arc::new(ConsumerMetrics::new("access"));
    if let Err(err) = access_consumer_metrics.register(registry) {
        eprintln!("failed to register access consumer metrics: {err}");
        return ExitCode::from(1);
    }
    let access_consumer = access_consumer.with_metrics(access_consumer_metrics);
    let access_consumer_name = format!("access-{}", uuid::Uuid::now_v7());
    let decision_cache: Arc<dyn sanvi_access::domain::DecisionCache> =
        Arc::new(InMemoryDecisionCache::new(10_000));
    let access_handler = sanvi_access::adapters::sync::access_sync_handler(
        access_store.clone(),
        Some(decision_cache.clone()),
        role_key_resolver,
    );

    // --- access events consumer: entitlement cache + role authoring sync ----
    let entitlements: Arc<dyn EntitlementRepository> = Arc::new(SqlxEntitlementRepository::new());
    let quota: Arc<dyn QuotaUsageStore> = Arc::new(SqlxQuotaUsageStore::new());
    let access_events: Arc<dyn EventPublisher> = Arc::new(AccessEventPublisher::new(ids.clone()));
    let entitlement_cache: Arc<dyn EntitlementCache> = Arc::new(RedisEntitlementCache::new(
        redis.clone(),
        config.access.entitlement_cache_ttl_secs,
    ));

    let entitlement_consumer = StreamConsumer::new(pool.clone(), streams.clone());
    let entitlement_consumer_metrics = Arc::new(ConsumerMetrics::new("access-entitlements"));
    if let Err(err) = entitlement_consumer_metrics.register(registry) {
        eprintln!("failed to register access-entitlements consumer metrics: {err}");
        return ExitCode::from(1);
    }
    let entitlement_consumer = entitlement_consumer.with_metrics(entitlement_consumer_metrics);
    let entitlement_consumer_name = format!("access-entitlements-{}", uuid::Uuid::now_v7());
    let entitlement_handler = entitlement_sync_handler(
        Some(entitlement_cache.clone()),
        Some(decision_cache),
        Some(access_store),
    );

    // --- billing context: Stripe webhooks, dunning, reconciliation -----------
    let billing_tx = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let billing_tenancy_events = Arc::new(TenancyEventPublisher::new(ids.clone()));
    let billing_plans: std::sync::Arc<dyn PlanRepository> =
        std::sync::Arc::new(SqlxPlanRepository::new());
    let billing_prices: std::sync::Arc<dyn PlanPriceRepository> =
        std::sync::Arc::new(SqlxPlanPriceRepository::new());
    let billing_grants: std::sync::Arc<dyn PlanEntitlementRepository> =
        std::sync::Arc::new(SqlxPlanEntitlementRepository::new());
    let billing_subscriptions: std::sync::Arc<dyn SubscriptionRepository> =
        std::sync::Arc::new(SqlxSubscriptionRepository::new());
    let billing_invoices: std::sync::Arc<dyn InvoiceRepository> =
        std::sync::Arc::new(SqlxInvoiceRepository::new());
    let billing_webhooks: std::sync::Arc<dyn WebhookEventRepository> =
        std::sync::Arc::new(SqlxWebhookEventRepository::new());
    let billing_events: std::sync::Arc<dyn BillingEvents> =
        std::sync::Arc::new(BillingEventPublisher::new(ids.clone()));
    let billing_notifications: std::sync::Arc<dyn Notifications> =
        std::sync::Arc::new(WorkerBillingNotifications {
            localizer: notification_localizer.clone(),
        });
    let billing_ids: std::sync::Arc<dyn BillingIds> =
        std::sync::Arc::new(SystemIdGenerator::new(ids.clone()));
    let billing_clock: std::sync::Arc<dyn BillingClock> = std::sync::Arc::new(SystemClock);
    let billing_syncer: std::sync::Arc<dyn EntitlementSyncer> = std::sync::Arc::new(
        AccessEntitlementSyncer::new(entitlements.clone(), ids.clone()),
    );
    let billing_lifecycle: std::sync::Arc<dyn sanvi_billing::domain::TenantLifecycle> =
        std::sync::Arc::new(TenancyTenantLifecycle::new(
            billing_tx.clone(),
            tenants.clone(),
            billing_tenancy_events,
            std::sync::Arc::new(TenancySystemClock),
        ));
    let billing_metrics = BillingMetrics::new(registry);
    let stripe_metrics = StripeMetrics::new(registry);
    let billing_stripe: std::sync::Arc<dyn StripeClient> =
        std::sync::Arc::new(HttpStripeClient::new(
            config.billing.stripe_api_base.clone(),
            config.billing.stripe_api_key.clone(),
            config.billing.stripe_api_version.clone(),
            Some(stripe_metrics),
        ));
    let billing_dispatcher = std::sync::Arc::new(
        WebhookDispatcher::new(
            billing_tx.clone(),
            billing_webhooks.clone(),
            billing_subscriptions.clone(),
            billing_invoices.clone(),
            billing_prices.clone(),
            billing_plans.clone(),
            billing_grants.clone(),
            billing_syncer.clone(),
            billing_lifecycle.clone(),
            billing_notifications.clone(),
            billing_events.clone(),
            billing_stripe.clone(),
            billing_ids.clone(),
            billing_clock.clone(),
        )
        .with_metrics(billing_metrics.clone()),
    );
    let dunning_sweep = std::sync::Arc::new(DunningSweep::new(
        billing_tx.clone(),
        billing_subscriptions.clone(),
        billing_syncer.clone(),
        billing_lifecycle.clone(),
        billing_notifications.clone(),
        billing_events.clone(),
        billing_clock.clone(),
        config.billing.dunning_grace_days,
        config.billing.trial_reminder_days,
    ));
    let reconciliation = std::sync::Arc::new(ReconcileSubscriptions::new(
        billing_tx.clone(),
        billing_subscriptions.clone(),
        billing_stripe.clone(),
        billing_dispatcher.clone(),
    ));
    let tax_check = std::sync::Arc::new(CheckTaxRegistrations::new(
        billing_stripe.clone(),
        config.billing.automatic_tax_enabled,
    ));

    // --- payments context (phase 09): Connect webhook dispatch, connection
    // reconciliation and gauges. The registry mirrors the API's: the fake
    // provider joins in non-production environments, the Stripe Connect
    // adapter when a restricted key is configured.
    let payments_tx = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let payments_connections_repo = std::sync::Arc::new(SqlxProviderConnectionRepository::new());
    let payments_lifecycle: std::sync::Arc<dyn ConnectionLifecycleStore> =
        payments_connections_repo.clone();
    let payments_webhooks: std::sync::Arc<dyn PaymentsWebhookEventRepository> =
        std::sync::Arc::new(PaymentsWebhookEvents::new());
    let payments_events: std::sync::Arc<dyn PaymentsEventPublisher> =
        std::sync::Arc::new(PaymentsEvents::new(ids.clone()));
    let payment_ids: std::sync::Arc<dyn sanvi_payments::domain::IdGenerator> = std::sync::Arc::new(
        sanvi_payments::adapters::SystemIdGenerator::new(ids.clone()),
    );
    let payments_notifier: std::sync::Arc<dyn PaymentsConnectionNotifier> =
        std::sync::Arc::new(WorkerPaymentsNotifications {
            localizer: notification_localizer.clone(),
        });
    let mut payments_providers = PaymentsProviderRegistry::new();
    if PaymentsRuntimeEnvironment::from_config(&config.telemetry.environment)
        == PaymentsRuntimeEnvironment::NonProduction
    {
        payments_providers
            .register(std::sync::Arc::new(
                sanvi_payments::adapters::FakeProvider::new(),
            ))
            .expect("the fake provider is the first registration");
    }
    if !config.payments.connect_api_key.is_empty() {
        payments_providers
            .register(std::sync::Arc::new(
                sanvi_payments::adapters::StripeConnectProvider::new(
                    config.payments.connect_api_base.clone(),
                    config.payments.connect_api_key.clone(),
                    config.payments.stripe_api_version.clone(),
                    config.payments.connect_webhook_secret.clone(),
                    config.payments.webhook_tolerance_secs,
                ),
            ))
            .expect("the stripe connect adapter registers");
    }
    let payments_registry = std::sync::Arc::new(payments_providers);
    let payments_metrics = sanvi_payments::adapters::PaymentsMetrics::new(registry);
    // 09.4: the payment-event handlers' persistence. Always wired, even
    // while checkout creation is flag-off — money taken at Stripe must
    // never land without a local record.
    let payments_checkouts: std::sync::Arc<dyn sanvi_payments::domain::CheckoutLifecycleStore> =
        std::sync::Arc::new(sanvi_payments::adapters::SqlxCheckoutRepository::new());
    let payments_refunds: std::sync::Arc<dyn sanvi_payments::domain::RefundRepository> =
        std::sync::Arc::new(sanvi_payments::adapters::SqlxRefundRepository::new());
    let payments_disputes: std::sync::Arc<dyn sanvi_payments::domain::DisputeRepository> =
        std::sync::Arc::new(sanvi_payments::adapters::SqlxDisputeRepository::new());
    // 09.6: the scheduled (and webhook-triggered) tax-compliance re-check.
    let payments_tax_settings: std::sync::Arc<dyn sanvi_payments::domain::TaxSettingsRepository> =
        std::sync::Arc::new(sanvi_payments::adapters::SqlxTaxSettingsRepository::new());
    let tax_recheck = std::sync::Arc::new(
        sanvi_payments::application::CheckTenantTaxCompliance::new(
            payments_tx.clone(),
            payments_connections_repo.clone(),
            payments_registry.clone(),
            payments_tax_settings.clone(),
            payments_notifier.clone(),
        )
        .with_metrics(payments_metrics.clone()),
    );
    let connect_dispatcher = std::sync::Arc::new(
        ConnectWebhookDispatcher::new(
            payments_tx.clone(),
            payments_webhooks.clone(),
            payments_lifecycle.clone(),
            payments_registry.clone(),
            payments_events.clone(),
            payments_notifier.clone(),
            payment_ids,
        )
        .with_checkout_store(payments_checkouts.clone())
        .with_refund_store(payments_refunds.clone())
        .with_dispute_store(payments_disputes.clone())
        .with_payout_store(std::sync::Arc::new(
            sanvi_payments::adapters::SqlxPayoutRepository::new(),
        ))
        .with_tax_checker(tax_recheck.clone())
        .with_metrics(payments_metrics.clone()),
    );
    let connection_sync = std::sync::Arc::new(
        SyncConnections::new(
            payments_tx.clone(),
            payments_lifecycle.clone(),
            payments_registry.clone(),
            payments_events.clone(),
            payments_notifier.clone(),
            chrono::Duration::seconds(
                config.scheduler.payments_connection_sync_interval_secs as i64,
            ),
        )
        .with_metrics(payments_metrics.clone()),
    );

    // --- billing events consumer: entitlement cache invalidation ------------
    let billing_consumer = StreamConsumer::new(pool.clone(), streams.clone());
    let billing_consumer_metrics = std::sync::Arc::new(ConsumerMetrics::new("billing"));
    if let Err(err) = billing_consumer_metrics.register(registry) {
        eprintln!("failed to register billing consumer metrics: {err}");
        return ExitCode::from(1);
    }
    let billing_consumer = billing_consumer.with_metrics(billing_consumer_metrics);
    let billing_consumer_name = format!("billing-{}", uuid::Uuid::now_v7());
    let billing_event_handler =
        sanvi_billing::adapters::billing_event_handler(entitlement_cache.clone());

    // --- domains context (phase 08): verification, issuance, renewal,
    // health and order-advancement sweeps + the resolution-cache consumer.
    let domains_tx = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let domains_repo: std::sync::Arc<dyn sanvi_domains::domain::CustomDomainRepository> =
        std::sync::Arc::new(sanvi_domains::adapters::SqlxCustomDomainRepository::new());
    let domains_orders: std::sync::Arc<dyn sanvi_domains::domain::DomainOrderRepository> =
        std::sync::Arc::new(sanvi_domains::adapters::SqlxDomainOrderRepository::new());
    let domains_certs: std::sync::Arc<dyn sanvi_domains::domain::CertificateRepository> =
        std::sync::Arc::new(sanvi_domains::adapters::SqlxCertificateRepository::new());
    let domains_records: std::sync::Arc<dyn sanvi_domains::domain::DnsRecordRepository> =
        std::sync::Arc::new(sanvi_domains::adapters::SqlxDnsRecordRepository::new(
            ids.clone(),
        ));
    let domains_provider: std::sync::Arc<dyn sanvi_domains::domain::DnsProviderPort> =
        std::sync::Arc::new(sanvi_domains::adapters::SqlxDnsProvider::new(
            domains_records.clone(),
        ));
    let domains_registrar: std::sync::Arc<dyn sanvi_domains::domain::DomainRegistrarPort> =
        std::sync::Arc::new(sanvi_domains::adapters::SandboxRegistrar::new(
            config.domains.registrar.registrar_id.clone(),
            &config.domains.registrar.tlds,
        ));
    let domains_ca: std::sync::Arc<dyn sanvi_domains::domain::CertificateAuthorityPort> =
        std::sync::Arc::new(sanvi_domains::adapters::SandboxCertificateAuthority::new(
            config.domains.cert_lifetime_days,
            ids.clone(),
        ));
    let domains_verifier: std::sync::Arc<dyn sanvi_domains::domain::DnsVerifierPort> =
        std::sync::Arc::new(sanvi_domains::adapters::DohDnsVerifier::new(
            config.domains.doh_resolvers.clone(),
            config.domains.doh_min_agreement,
            config.domains.doh_timeout_secs,
        ));
    let domains_prober: std::sync::Arc<dyn sanvi_domains::domain::StorefrontProbePort> =
        std::sync::Arc::new(sanvi_domains::adapters::HttpProbeAdapter::new(
            config.domains.prober_timeout_secs,
            config.domains.prober_user_agent.clone(),
        ));
    let domains_events: std::sync::Arc<dyn sanvi_domains::domain::EventPublisher> =
        std::sync::Arc::new(sanvi_domains::adapters::OutboxEventPublisher::new(
            ids.clone(),
        ));
    let domains_clock: std::sync::Arc<dyn sanvi_domains::domain::Clock> =
        std::sync::Arc::new(sanvi_domains::adapters::SystemClock);
    let domains_notifications: std::sync::Arc<dyn sanvi_domains::domain::Notifications> =
        std::sync::Arc::new(sanvi_domains::adapters::LogNotifications::new());
    let domains_metrics = std::sync::Arc::new(sanvi_domains::adapters::DomainsMetrics::new(
        metrics.prometheus_registry(),
    ));

    // The worker's charger is the same sandbox adapter the API uses (the
    // purchase saga's charge step is a port; both processes implement it).
    let domains_charger: std::sync::Arc<dyn sanvi_domains::domain::PurchaseCharger> =
        std::sync::Arc::new(sanvi_domains::adapters::SandboxPurchaseCharger);

    let domains_place_order =
        std::sync::Arc::new(sanvi_domains::application::PlaceDomainOrder::new(
            domains_orders.clone(),
            domains_repo.clone(),
            domains_registrar.clone(),
            domains_provider.clone(),
            domains_charger.clone(),
            std::sync::Arc::new(sanvi_domains::adapters::AlwaysDisabledGate),
            domains_events.clone(),
            domains_clock.clone(),
            ids.clone(),
            domains_tx.clone(),
            config.domains.platform_domain.clone(),
            config.domains.edge_hostname.clone(),
            config.domains.anycast_ips.clone(),
            config.domains.purchase_entitlement_key.clone(),
            config.domains.verification_timeout_hours,
            config.domains.verification_backoff_base_secs,
        ));
    let domains_verification_sweep =
        std::sync::Arc::new(sanvi_domains::application::RunVerificationSweep::new(
            domains_repo.clone(),
            domains_verifier.clone(),
            domains_provider.clone(),
            domains_events.clone(),
            domains_tx.clone(),
            config.domains.edge_hostname.clone(),
            config.domains.anycast_ips.clone(),
            config.domains.verification_backoff_base_secs,
            config.domains.verification_backoff_max_secs,
        ));
    let domains_cert_sweep =
        std::sync::Arc::new(sanvi_domains::application::RunCertIssuanceSweep::new(
            domains_repo.clone(),
            domains_certs.clone(),
            domains_ca.clone(),
            domains_events.clone(),
            domains_clock.clone(),
            domains_tx.clone(),
        ));
    let domains_renewal_sweep =
        std::sync::Arc::new(sanvi_domains::application::RunRenewalSweep::new(
            domains_repo.clone(),
            domains_certs.clone(),
            domains_orders.clone(),
            domains_ca.clone(),
            domains_registrar.clone(),
            domains_charger.clone(),
            domains_events.clone(),
            domains_notifications.clone(),
            domains_tx.clone(),
            config.domains.cert_renewal_before_days,
            config.domains.cert_expiry_warning_days.clone(),
            config.domains.order_renewal_lead_days,
            config.domains.order_renewal_retry_secs,
            config.domains.order_renewal_max_attempts,
        ));
    let domains_health_sweep =
        std::sync::Arc::new(sanvi_domains::application::RunHealthSweep::new(
            domains_repo.clone(),
            domains_certs.clone(),
            domains_verifier.clone(),
            domains_prober.clone(),
            domains_events.clone(),
            domains_notifications.clone(),
            domains_clock.clone(),
            domains_tx.clone(),
            config.domains.edge_hostname.clone(),
            config.domains.anycast_ips.clone(),
        ));
    let domains_order_sweep = std::sync::Arc::new(sanvi_domains::application::RunOrderSweep::new(
        domains_orders.clone(),
        domains_registrar.clone(),
        domains_charger.clone(),
        domains_place_order.clone(),
        domains_tx.clone(),
    ));
    let domains_cache = std::sync::Arc::new(sanvi_domains::adapters::LiveDomainCache::new(
        Some(redis.clone()),
        config.domains.host_cache_ttl_secs,
    ));

    // --- privacy context (phase 05): DSR saga + compliance jobs --------------
    let privacy_tx = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let privacy_subjects: std::sync::Arc<dyn SubjectRepository> =
        std::sync::Arc::new(SqlxSubjectRepository::new());
    let privacy_jurisdictions: std::sync::Arc<dyn JurisdictionRepository> =
        std::sync::Arc::new(SqlxJurisdictionRepository::new());
    let privacy_requests: std::sync::Arc<dyn DsrRepository> =
        std::sync::Arc::new(SqlxDsrRepository::new());
    let privacy_appeals: std::sync::Arc<dyn AppealRepository> =
        std::sync::Arc::new(SqlxAppealRepository::new());
    let privacy_participants: std::sync::Arc<dyn ParticipantRepository> =
        std::sync::Arc::new(SqlxParticipantRepository::new());
    let privacy_consents: std::sync::Arc<dyn ConsentRepository> =
        std::sync::Arc::new(SqlxConsentRepository::new());
    let privacy_signals: std::sync::Arc<dyn SignalRepository> =
        std::sync::Arc::new(SqlxSignalRepository::new());
    let privacy_directives: std::sync::Arc<dyn DirectiveRepository> =
        std::sync::Arc::new(SqlxDirectiveRepository::new());
    let privacy_holds: std::sync::Arc<dyn LegalHoldRepository> =
        std::sync::Arc::new(SqlxLegalHoldRepository::new());
    let privacy_keys: std::sync::Arc<dyn SubjectKeyRepository> =
        std::sync::Arc::new(SqlxSubjectKeyRepository::new());
    let privacy_artifacts: std::sync::Arc<dyn ExportArtifactRepository> =
        std::sync::Arc::new(SqlxExportArtifactRepository::new());
    let privacy_challenges: std::sync::Arc<dyn VerificationRepository> =
        std::sync::Arc::new(SqlxVerificationRepository::new());
    let privacy_agents: std::sync::Arc<dyn AuthorizedAgentRepository> =
        std::sync::Arc::new(SqlxAuthorizedAgentRepository::new());
    let privacy_processors: std::sync::Arc<dyn SubProcessorRepository> =
        std::sync::Arc::new(SqlxSubProcessorRepository::new());
    let privacy_breaches: std::sync::Arc<dyn BreachRepository> =
        std::sync::Arc::new(SqlxBreachRepository::new());
    let privacy_rules: std::sync::Arc<dyn RetentionRuleRepository> =
        std::sync::Arc::new(SqlxRetentionRuleRepository::new());
    let privacy_metrics: std::sync::Arc<dyn RequestMetricsRepository> =
        std::sync::Arc::new(SqlxRequestMetricsRepository::new());
    let privacy_events = std::sync::Arc::new(PrivacyEvents::new(ids.clone()));
    let privacy_notifications: std::sync::Arc<dyn PrivacyNotificationsTrait> =
        std::sync::Arc::new(WorkerPrivacyNotifications {
            localizer: notification_localizer.clone(),
        });
    let privacy_clock = std::sync::Arc::new(PrivacySystemClock);
    let privacy_ids: std::sync::Arc<dyn sanvi_privacy::domain::IdGenerator> =
        std::sync::Arc::new(PrivacySystemIdGenerator::new(ids.clone()));
    let privacy_hasher: std::sync::Arc<dyn Hasher> =
        std::sync::Arc::new(Sha256Hasher::new(if config.privacy.kek.is_empty() {
            "dev-pepper-insecure".to_string()
        } else {
            config.privacy.kek.clone()
        }));
    let privacy_crypto: std::sync::Arc<dyn EnvelopeCrypto> = if config.privacy.encryption_enabled()
    {
        let kek: [u8; 32] = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            &config.privacy.kek,
        )
        .expect("validated by PrivacyConfig")
        .try_into()
        .expect("validated length");
        std::sync::Arc::new(SqlxEnvelopeCrypto::new(
            privacy_tx.clone(),
            privacy_keys.clone(),
            kek,
            config.privacy.kek_id.clone(),
        ))
    } else {
        std::sync::Arc::new(DisabledEnvelopeCrypto)
    };
    let privacy_notifier: std::sync::Arc<dyn SubProcessorNotifier> =
        std::sync::Arc::new(LoggingSubProcessorNotifier);

    // The provider registry: the same eight contexts the CI test enforces.
    // The worker rebuilds it (providers are stateless ZSTs + repos).
    let privacy_providers: ProviderRegistry = vec![
        std::sync::Arc::new(sanvi_system::adapters::SystemPrivacyProvider::new()),
        std::sync::Arc::new(
            sanvi_tenancy::adapters::privacy_provider::TenancyPrivacyProvider::new(
                privacy_tx.clone(),
                std::sync::Arc::new(sanvi_tenancy::adapters::repo::SqlxTenantRepository::new()),
            ),
        ),
        std::sync::Arc::new(
            sanvi_identity::adapters::privacy_provider::IdentityPrivacyProvider::new(
                privacy_tx.clone(),
                std::sync::Arc::new(sanvi_identity::adapters::repo::SqlxUserRepository::new()),
                std::sync::Arc::new(
                    sanvi_identity::adapters::repo::SqlxMembershipRepository::new(),
                ),
                std::sync::Arc::new(
                    sanvi_identity::adapters::repo::SqlxLinkedAccountRepository::new(),
                ),
                std::sync::Arc::new(
                    sanvi_identity::adapters::repo::SqlxInvitationRepository::new(),
                ),
                std::sync::Arc::new(
                    sanvi_identity::adapters::repo::SqlxLinkIntentRepository::new(),
                ),
                std::sync::Arc::new(
                    sanvi_identity::adapters::HttpKratosClient::new(
                        config.ory.kratos_public_url.clone(),
                        Some(config.ory.kratos_admin_url.clone()),
                        config.identity.session_cookie_name.clone(),
                    )
                    .expect("kratos client construction is infallible"),
                ),
                std::sync::Arc::new(sanvi_identity::adapters::SystemClock),
            ),
        ),
        std::sync::Arc::new(
            sanvi_access::adapters::privacy_provider::AccessPrivacyProvider::new(
                privacy_tx.clone(),
                std::sync::Arc::new(sanvi_access::adapters::SqlxEntitlementRepository::new()),
                match sanvi_access::adapters::HttpKetoClient::new(
                    config.ory.keto_read_url.clone(),
                    config.ory.keto_write_url.clone(),
                ) {
                    Ok(client) => std::sync::Arc::new(client),
                    Err(err) => {
                        eprintln!("failed to build the keto client: {err}");
                        return ExitCode::from(2);
                    }
                },
            ),
        ),
        std::sync::Arc::new(sanvi_audit::adapters::AuditPrivacyProvider::new(
            privacy_tx.clone(),
            std::sync::Arc::new(SqlxAuditReader::new()),
            std::sync::Arc::new(SqlxAuditWriter::new(ids.clone())),
        )),
        std::sync::Arc::new(
            sanvi_platform_admin::adapters::privacy_provider::PlatformAdminPrivacyProvider::new(
                privacy_tx.clone(),
                std::sync::Arc::new(
                    sanvi_platform_admin::adapters::SqlxImpersonationGrantRepository::new(),
                ),
                std::sync::Arc::new(
                    sanvi_platform_admin::adapters::SqlxPendingApprovalRepository::new(),
                ),
                std::sync::Arc::new(sanvi_platform_admin::adapters::NoopPrivacyLock),
            ),
        ),
        std::sync::Arc::new(
            sanvi_billing::adapters::privacy_provider::BillingPrivacyProvider::new(
                privacy_tx.clone(),
                std::sync::Arc::new(sanvi_billing::adapters::SqlxSubscriptionRepository::new()),
                std::sync::Arc::new(sanvi_billing::adapters::SqlxCheckoutStateRepository::new()),
                std::sync::Arc::new(sanvi_billing::adapters::SqlxInvoiceRepository::new()),
                std::sync::Arc::new(sanvi_billing::adapters::SqlxWebhookEventRepository::new()),
                billing_stripe.clone(),
            ),
        ),
        std::sync::Arc::new(
            sanvi_localization::adapters::LocalizationPrivacyProvider::new(privacy_tx.clone()),
        ),
        std::sync::Arc::new(
            sanvi_theming::adapters::privacy_provider::ThemingPrivacyProvider::new(
                privacy_tx.clone(),
            ),
        ),
        std::sync::Arc::new(
            sanvi_domains::adapters::privacy_provider::DomainsPrivacyProvider::new(
                privacy_tx.clone(),
            ),
        ),
        std::sync::Arc::new(PrivacyContextProvider::new(
            privacy_tx.clone(),
            privacy_subjects.clone(),
            privacy_consents.clone(),
            privacy_signals.clone(),
            privacy_directives.clone(),
        )),
        std::sync::Arc::new(sanvi_payments::adapters::PaymentsPrivacyProvider::new(
            privacy_tx.clone(),
        )),
        std::sync::Arc::new(sanvi_advertising::adapters::AdvertisingPrivacyProvider::new()),
    ];

    let dsr_process = std::sync::Arc::new(ProcessDsrBatch::new(
        privacy_tx.clone(),
        privacy_requests.clone(),
        privacy_participants.clone(),
        privacy_holds.clone(),
        privacy_artifacts.clone(),
        privacy_providers.clone(),
        std::sync::Arc::new(ZipPackager::new()),
        privacy_crypto.clone(),
        privacy_notifications.clone(),
        privacy_events.clone(),
        privacy_hasher.clone(),
        privacy_ids,
        privacy_clock.clone(),
        config.privacy.export_dir.clone(),
        config.privacy.export_ttl_days,
        config.privacy.export_max_downloads,
        config.privacy.backup_retention_days,
    ));
    let optout_propagation = std::sync::Arc::new(PropagateOptOuts::new(
        privacy_tx.clone(),
        privacy_consents.clone(),
        privacy_processors.clone(),
        privacy_notifier,
    ));
    let retention_sweep = std::sync::Arc::new(RetentionSweep::new(
        privacy_tx.clone(),
        privacy_rules.clone(),
        privacy_consents.clone(),
        privacy_signals.clone(),
        privacy_clock.clone(),
        std::sync::Arc::new(sanvi_audit::adapters::PrivacyAuditWriterAdapter::new(
            std::sync::Arc::new(SqlxAuditWriter::new(ids.clone())),
        )),
        config.privacy.retention_dry_run,
        config.privacy.retention_sweep_batch_size,
    ));
    let export_cleanup = std::sync::Arc::new(CleanupExpiredExports::new(
        privacy_tx.clone(),
        privacy_artifacts.clone(),
    ));
    let annual_metrics = std::sync::Arc::new(GenerateAnnualMetrics::new(
        privacy_tx.clone(),
        privacy_requests.clone(),
        privacy_metrics.clone(),
        privacy_clock.clone(),
    ));
    let privacy_directive_resolver: std::sync::Arc<dyn DirectiveResolver> =
        std::sync::Arc::new(PersistedDirectiveResolver::new(
            privacy_tx.clone(),
            privacy_subjects.clone(),
            privacy_jurisdictions.clone(),
            privacy_consents.clone(),
            privacy_signals.clone(),
            privacy_directives.clone(),
            privacy_clock.clone(),
        ));
    let _ = (
        privacy_jurisdictions,
        privacy_appeals,
        privacy_challenges,
        privacy_agents,
        privacy_breaches,
        privacy_rules,
        privacy_metrics,
    );

    // --- scheduler: purge + idempotency + quota rollover + expiry + audit ----
    let events = Arc::new(TenancyEventPublisher::new(ids.clone()));
    let clock = Arc::new(TenancySystemClock);
    let purge_sweep = Arc::new(PurgeSweep::new(
        tx_manager.clone(),
        tenants.clone(),
        events,
        clock,
        config.scheduler.purge_retention_days,
    ));
    let idempotency = Arc::new(sanvi_persistence::IdempotencyStore::new(pool.clone()));
    let expire_entitlements = Arc::new(ExpireEntitlements::new(
        tx_manager.clone(),
        entitlements.clone(),
        entitlement_cache.clone(),
        access_events.clone(),
        Arc::new(sanvi_access::adapters::SystemClock),
    ));
    let audit_reader = Arc::new(SqlxAuditReader::new());
    let _audit_writer = SqlxAuditWriter::new(ids.clone());
    let verify_audit = Arc::new(VerifyAuditChain::new(
        tx_manager.clone(),
        audit_reader,
        config.audit.verify_batch_size,
    ));

    // TASK-012 (10.3): the nightly reconciliation sweep. The fake adapter is
    // the only registered platform until the real Google Ads adapter lands;
    // this still proves the sweep's own contract (fetch → apply through
    // ReconcileCampaign → never write back) end to end.
    let advertising_campaigns: Arc<dyn AdvertisingCampaignRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let advertising_events: Arc<dyn AdvertisingEvents> =
        Arc::new(AdvertisingEventPublisher::new(ids.clone()));
    let advertising_ids: Arc<dyn sanvi_advertising::domain::IdGenerator> = Arc::new(
        sanvi_advertising::adapters::SystemIdGenerator::new(ids.clone()),
    );
    let advertising_platforms = Arc::new(sanvi_advertising::adapters::rate_limited_registry(
        &FakeAdPlatform::registered_catalog(),
        |platform| match platform.as_str() {
            "meta" => (10, 2),
            _ => (
                config.advertising.quota_platform_wide_permits,
                config.advertising.quota_per_connection_permits,
            ),
        },
    ));
    let advertising_reconcile_campaign = Arc::new(ReconcileCampaign::new(
        tx_manager.clone(),
        advertising_campaigns.clone(),
        advertising_events,
        advertising_ids,
    ));
    let advertising_reconciliation_sweep = Arc::new(ReconcileDueCampaigns::new(
        tx_manager.clone(),
        advertising_campaigns,
        advertising_platforms,
        advertising_reconcile_campaign,
    ));
    let advertising_connections: Arc<dyn AdvertisingConnectionRepository> =
        Arc::new(SqlxAdvertisingConnectionRepository::new());
    let mut advertising_conversion_sinks: std::collections::BTreeMap<
        PlatformKey,
        Arc<dyn ConversionSinkPort>,
    > = std::collections::BTreeMap::new();
    for descriptor in FakeAdPlatform::fixtures().into_iter().take(2) {
        advertising_conversion_sinks.insert(
            descriptor.key.clone(),
            Arc::new(FakeConversionSink::new(descriptor)),
        );
    }
    let advertising_conversion_events: Arc<dyn ConversionEventRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let advertising_metrics = Arc::new(AdvertisingMetrics::new(registry));
    let advertising_conversion_upload_sweep = Arc::new(UploadPendingConversions::new(
        tx_manager.clone(),
        advertising_conversion_events.clone(),
        advertising_connections.clone(),
        privacy_directive_resolver.clone(),
        advertising_conversion_sinks.clone(),
        advertising_metrics.clone(),
    ));
    let advertising_conversion_retraction_sweep = Arc::new(RetractWithdrawnConversions::new(
        tx_manager.clone(),
        advertising_conversion_events,
        advertising_connections,
        privacy_directive_resolver,
        advertising_conversion_sinks,
        advertising_metrics,
    ));

    let jobs: Vec<Arc<dyn sanvi_messaging::Job>> = vec![
        Arc::new(FnJob::new(
            "advertising.partition_maintenance",
            Duration::from_secs(
                config
                    .scheduler
                    .advertising_partition_maintenance_interval_secs,
            ),
            {
                let pool = pool.clone();
                move || {
                    let pool = pool.clone();
                    Box::pin(async move {
                        sqlx::query(
                            "SELECT advertising.ensure_monthly_partitions((current_date + interval '1 month')::date)",
                        )
                        .execute(&pool)
                        .await?;
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "advertising.reconciliation",
            Duration::from_secs(config.scheduler.advertising_reconciliation_interval_secs),
            {
                let sweep = advertising_reconciliation_sweep.clone();
                move || {
                    let sweep = sweep.clone();
                    Box::pin(async move {
                        let report = sweep
                            .execute(200)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if report.checked > 0 || report.errors > 0 {
                            tracing::info!(
                                checked = report.checked,
                                updated = report.updated,
                                drifted = report.drifted,
                                errors = report.errors,
                                "advertising reconciliation sweep pass"
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "advertising.conversion_upload",
            Duration::from_secs(config.scheduler.advertising_conversion_upload_interval_secs),
            {
                let sweep = advertising_conversion_upload_sweep.clone();
                move || {
                    let sweep = sweep.clone();
                    Box::pin(async move {
                        let report = sweep
                            .execute(200)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if report.checked > 0 {
                            tracing::info!(
                                checked = report.checked,
                                uploaded = report.uploaded,
                                failed = report.failed,
                                parked = report.parked,
                                suppressed_late = report.suppressed_late,
                                "advertising conversion upload sweep pass"
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "advertising.conversion_retraction",
            Duration::from_secs(config.scheduler.advertising_conversion_retraction_interval_secs),
            {
                let sweep = advertising_conversion_retraction_sweep.clone();
                move || {
                    let sweep = sweep.clone();
                    Box::pin(async move {
                        let report = sweep
                            .execute(200)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if report.checked > 0 {
                            tracing::info!(
                                checked = report.checked,
                                retracted = report.retracted,
                                unpropagated = report.unpropagated,
                                failed = report.failed,
                                "advertising conversion retraction sweep pass"
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "tenancy.purge_sweep",
            Duration::from_secs(config.scheduler.purge_sweep_interval_secs),
            {
                let sweep = purge_sweep.clone();
                move || {
                    let sweep = sweep.clone();
                    Box::pin(async move {
                        sweep
                            .execute(100)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "platform.idempotency_sweep",
            Duration::from_secs(config.scheduler.idempotency_sweep_interval_secs),
            {
                let store = idempotency.clone();
                move || {
                    let store = store.clone();
                    Box::pin(async move {
                        let deleted = store.sweep_expired(Duration::from_secs(0)).await?;
                        if deleted > 0 {
                            tracing::info!(deleted, "idempotency sweep removed expired keys");
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "access.quota_rollover",
            Duration::from_secs(config.scheduler.quota_rollover_interval_secs),
            {
                let quota = quota.clone();
                let tx = tx_manager.clone();
                move || {
                    let quota = quota.clone();
                    let tx = tx.clone();
                    Box::pin(async move {
                        // Keep only the current period: usage resets monthly.
                        let now = sanvi_access::domain::period_start(
                            sanvi_access::adapters::SystemClock.now(),
                        );
                        let mut scope = tx.begin_unscoped("quota_rollover", "access").await?;
                        let removed = quota.rollover(&mut scope, now).await?;
                        scope.commit().await?;
                        if removed > 0 {
                            tracing::info!(removed, "quota rollover removed old periods");
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "access.entitlement_expiry_sweep",
            Duration::from_secs(config.scheduler.entitlement_expiry_sweep_interval_secs),
            {
                let sweep = expire_entitlements.clone();
                move || {
                    let sweep = sweep.clone();
                    Box::pin(async move {
                        let expired = sweep
                            .execute(500)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if expired > 0 {
                            tracing::info!(expired, "entitlement sweep removed expired overrides");
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "audit.verify_chain",
            Duration::from_secs(config.scheduler.audit_verify_interval_secs),
            {
                let verify = verify_audit.clone();
                move || {
                    let verify = verify.clone();
                    Box::pin(async move {
                        let report = verify.execute().await.map_err(|err| anyhow::anyhow!(err))?;
                        if report.verified {
                            tracing::info!(
                                entries = report.entries_checked,
                                "audit chain verified"
                            );
                        } else {
                            // A tampered chain is a security incident: alert
                            // immediately (the log level + metric fire the
                            // alerting rule).
                            tracing::error!(
                                entries = report.entries_checked,
                                mismatch = %report.first_mismatch.unwrap_or_default(),
                                "AUDIT CHAIN VERIFICATION FAILED — possible tampering"
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        // --- phase 04: billing -------------------------------------------------
        Arc::new(FnJob::new(
            "billing.webhook_dispatch",
            Duration::from_secs(config.scheduler.webhook_dispatch_interval_secs),
            {
                let dispatcher = billing_dispatcher.clone();
                move || {
                    let dispatcher = dispatcher.clone();
                    Box::pin(async move {
                        let report = dispatcher
                            .dispatch(100)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if report.processed > 0 || report.skipped > 0 || report.failed > 0 {
                            tracing::info!(
                                processed = report.processed,
                                skipped = report.skipped,
                                failed = report.failed,
                                "stripe webhook dispatch pass",
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "billing.dunning_sweep",
            Duration::from_secs(config.scheduler.dunning_sweep_interval_secs),
            {
                let sweep = dunning_sweep.clone();
                move || {
                    let sweep = sweep.clone();
                    Box::pin(async move {
                        let report = sweep
                            .execute(500)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if report.grace_expired > 0 || report.trials_notified > 0 {
                            tracing::info!(
                                grace_expired = report.grace_expired,
                                trials_notified = report.trials_notified,
                                "billing dunning sweep",
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "billing.reconciliation",
            Duration::from_secs(config.scheduler.reconciliation_interval_secs),
            {
                let reconciliation = reconciliation.clone();
                let metrics = billing_metrics.clone();
                move || {
                    let reconciliation = reconciliation.clone();
                    let metrics = metrics.clone();
                    Box::pin(async move {
                        let report = reconciliation
                            .execute(500)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        metrics.drift_total().inc_by(report.drift_repaired as u64);
                        tracing::info!(
                            checked = report.checked,
                            drift_repaired = report.drift_repaired,
                            errors = report.errors,
                            "billing reconciliation pass",
                        );
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "billing.tax_check",
            Duration::from_secs(config.scheduler.tax_check_interval_secs),
            {
                let tax_check = tax_check.clone();
                move || {
                    let tax_check = tax_check.clone();
                    Box::pin(async move {
                        // Errors already log at error level inside the use
                        // case (the alerting rule fires on them).
                        let _ = tax_check.execute().await;
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "billing.metrics",
            Duration::from_secs(config.scheduler.billing_metrics_interval_secs),
            {
                let tx = billing_tx.clone();
                let subscriptions = billing_subscriptions.clone();
                let webhooks = billing_webhooks.clone();
                let metrics = billing_metrics.clone();
                move || {
                    let tx = tx.clone();
                    let subscriptions = subscriptions.clone();
                    let webhooks = webhooks.clone();
                    let metrics = metrics.clone();
                    Box::pin(async move {
                        let mut scope = tx
                            .begin_unscoped("billing_metrics", "worker")
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        metrics
                            .unprocessed()
                            .set(webhooks.unprocessed_count(&mut scope).await? as i64);
                        metrics
                            .dead_letter_total()
                            .set(webhooks.dead_letter_count(&mut scope).await? as i64);
                        metrics
                            .dunning_active()
                            .set(subscriptions.dunning_count(&mut scope).await? as i64);
                        metrics.mrr().reset();
                        for (currency, amount) in subscriptions.mrr_by_currency(&mut scope).await? {
                            metrics.mrr().with_label_values(&[&currency]).set(amount);
                        }
                        metrics.status_gauge().reset();
                        for (status, count) in subscriptions.count_by_status(&mut scope).await? {
                            metrics
                                .status_gauge()
                                .with_label_values(&[status.as_str()])
                                .set(count);
                        }
                        scope.commit().await.map_err(|err| anyhow::anyhow!(err))?;
                        Ok(())
                    })
                }
            },
        )),
        // --- phase 09: payments -------------------------------------------------
        Arc::new(FnJob::new(
            "payments.webhook_dispatch",
            Duration::from_secs(config.scheduler.payments_webhook_dispatch_interval_secs),
            {
                let dispatcher = connect_dispatcher.clone();
                move || {
                    let dispatcher = dispatcher.clone();
                    Box::pin(async move {
                        let report = dispatcher
                            .dispatch(sanvi_payments::application::DISPATCH_BATCH)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if report.processed > 0 || report.skipped > 0 || report.failed > 0 {
                            tracing::info!(
                                processed = report.processed,
                                skipped = report.skipped,
                                failed = report.failed,
                                "connect webhook dispatch pass",
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "payments.connection_sync",
            Duration::from_secs(config.scheduler.payments_connection_sync_interval_secs),
            {
                let sync = connection_sync.clone();
                move || {
                    let sync = sync.clone();
                    Box::pin(async move {
                        // The safety net for missed webhooks: a lost
                        // delivery becomes a delay (one sync interval),
                        // never a stuck connection.
                        let report = sync
                            .execute(200)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if report.checked > 0 || report.errors > 0 {
                            tracing::info!(
                                checked = report.checked,
                                updated = report.updated,
                                errors = report.errors,
                                "connection reconciliation pass",
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "payments.metrics",
            Duration::from_secs(config.scheduler.payments_metrics_interval_secs),
            {
                let tx = payments_tx.clone();
                let webhooks = payments_webhooks.clone();
                let lifecycle = payments_lifecycle.clone();
                let disputes = payments_disputes.clone();
                let metrics = payments_metrics.clone();
                move || {
                    let tx = tx.clone();
                    let webhooks = webhooks.clone();
                    let lifecycle = lifecycle.clone();
                    let disputes = disputes.clone();
                    let metrics = metrics.clone();
                    Box::pin(async move {
                        let mut scope = tx
                            .begin_unscoped("payments_metrics", "worker")
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        metrics
                            .webhook_unprocessed
                            .set(webhooks.unprocessed_count(&mut scope).await? as i64);
                        metrics
                            .webhook_dead_letter
                            .set(webhooks.dead_letter_count(&mut scope).await? as i64);
                        metrics
                            .dispute_open
                            .set(disputes.count_open(&mut scope).await?);
                        metrics.connection_status.reset();
                        for (status, count) in lifecycle.count_by_status(&mut scope).await? {
                            metrics
                                .connection_status
                                .with_label_values(&[&status])
                                .set(count);
                        }
                        scope.commit().await.map_err(|err| anyhow::anyhow!(err))?;
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "payments.tax_check",
            Duration::from_secs(config.scheduler.payments_tax_check_interval_secs),
            {
                let recheck = tax_recheck.clone();
                move || {
                    let recheck = recheck.clone();
                    Box::pin(async move {
                        let report = recheck
                            .execute(500)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if report.checked > 0 || report.warned > 0 || report.errors > 0 {
                            tracing::info!(
                                checked = report.checked,
                                warned = report.warned,
                                errors = report.errors,
                                "tax compliance re-check pass",
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        // --- phase 08: domains -------------------------------------------------
        Arc::new(FnJob::new(
            "domains.verification_poll",
            Duration::from_secs(config.scheduler.domains_verification_interval_secs),
            {
                let sweep = domains_verification_sweep.clone();
                let clock = domains_clock.clone();
                move || {
                    let sweep = sweep.clone();
                    let clock = clock.clone();
                    Box::pin(async move {
                        let now = clock.now();
                        let report = sweep
                            .execute(now, 200)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if report.checked > 0 {
                            tracing::info!(
                                checked = report.checked,
                                verified = report.verified,
                                failed = report.failed,
                                expired = report.expired,
                                superseded = report.superseded,
                                "domains verification pass",
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "domains.cert_issue",
            Duration::from_secs(config.scheduler.domains_cert_issue_interval_secs),
            {
                let sweep = domains_cert_sweep.clone();
                let metrics = domains_metrics.clone();
                move || {
                    let sweep = sweep.clone();
                    let metrics = metrics.clone();
                    Box::pin(async move {
                        let report = sweep
                            .execute(100)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        for _ in 0..report.issued {
                            metrics.acme_issue_total.with_label_values(&["ok"]).inc();
                        }
                        for _ in 0..report.failures {
                            metrics.acme_issue_total.with_label_values(&["error"]).inc();
                        }
                        if report.issued > 0 || report.failures > 0 {
                            tracing::info!(
                                issued = report.issued,
                                failures = report.failures,
                                "domains certificate issuance pass",
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "domains.renewal_sweep",
            Duration::from_secs(config.scheduler.domains_renewal_interval_secs),
            {
                let sweep = domains_renewal_sweep.clone();
                let clock = domains_clock.clone();
                move || {
                    let sweep = sweep.clone();
                    let clock = clock.clone();
                    Box::pin(async move {
                        let now = clock.now();
                        let report = sweep
                            .execute(now, 500)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if report.cert_renewal_failures > 0 || report.order_renewals_escalated > 0 {
                            tracing::error!(
                                cert_renewal_failures = report.cert_renewal_failures,
                                order_renewals_escalated = report.order_renewals_escalated,
                                "domains renewal escalation",
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "domains.health_check",
            Duration::from_secs(config.scheduler.domains_health_interval_secs),
            {
                let sweep = domains_health_sweep.clone();
                move || {
                    let sweep = sweep.clone();
                    Box::pin(async move {
                        let report = sweep
                            .execute(500)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if report.checked > 0 {
                            tracing::info!(
                                checked = report.checked,
                                degraded = report.degraded,
                                recovered = report.recovered,
                                "domains health pass",
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "domains.order_sweep",
            Duration::from_secs(config.scheduler.domains_orders_interval_secs),
            {
                let sweep = domains_order_sweep.clone();
                move || {
                    let sweep = sweep.clone();
                    Box::pin(async move {
                        let report = sweep
                            .execute(100)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if report.resumed > 0
                            || report.configured > 0
                            || report.refunded > 0
                            || report.failed > 0
                        {
                            tracing::info!(
                                resumed = report.resumed,
                                configured = report.configured,
                                refunded = report.refunded,
                                failed = report.failed,
                                "domains order sweep",
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        // --- phase 05: privacy --------------------------------------------------
        Arc::new(FnJob::new(
            "privacy.dsr_advance",
            Duration::from_secs(config.scheduler.dsr_advance_interval_secs),
            {
                let process = dsr_process.clone();
                move || {
                    let process = process.clone();
                    Box::pin(async move {
                        let report = process
                            .execute(100)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if report.advanced > 0 || report.escalated > 0 {
                            tracing::info!(
                                advanced = report.advanced,
                                completed = report.completed,
                                partially_complete = report.partially_complete,
                                failed = report.failed,
                                escalated = report.escalated,
                                shreds = report.shreds,
                                exports = report.exports_packaged,
                                "privacy DSR saga pass",
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "privacy.optout_propagation",
            Duration::from_secs(config.scheduler.optout_propagation_interval_secs),
            {
                let propagate = optout_propagation.clone();
                move || {
                    let propagate = propagate.clone();
                    Box::pin(async move {
                        let report = propagate
                            .execute(100)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if report.propagated > 0 || report.failed > 0 {
                            tracing::info!(
                                propagated = report.propagated,
                                failed = report.failed,
                                "opt-out propagation pass",
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "privacy.retention_sweep",
            Duration::from_secs(config.scheduler.retention_sweep_interval_secs),
            {
                let sweep = retention_sweep.clone();
                move || {
                    let sweep = sweep.clone();
                    Box::pin(async move {
                        let report = sweep.execute().await.map_err(|err| anyhow::anyhow!(err))?;
                        tracing::info!(
                            dry_run = report.dry_run,
                            scanned_consents = report.scanned.consents,
                            deleted_consents = report.deleted.consents,
                            deleted_signals = report.deleted.preference_signals,
                            anonymised = report.deleted.anonymised,
                            "retention sweep pass",
                        );
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "privacy.export_cleanup",
            Duration::from_secs(config.scheduler.export_cleanup_interval_secs),
            {
                let cleanup = export_cleanup.clone();
                move || {
                    let cleanup = cleanup.clone();
                    Box::pin(async move {
                        let removed = cleanup
                            .execute(500)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if removed > 0 {
                            tracing::info!(removed, "expired export artifacts removed");
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "privacy.annual_metrics",
            Duration::from_secs(config.scheduler.privacy_metrics_interval_secs),
            {
                let generate = annual_metrics.clone();
                move || {
                    let generate = generate.clone();
                    Box::pin(async move {
                        let rows = generate
                            .execute(None)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if rows > 0 {
                            tracing::info!(rows, "annual request metrics generated");
                        }
                        Ok(())
                    })
                }
            },
        )),
    ];
    let scheduler = Scheduler::new(
        redis.clone(),
        format!("worker-{}", uuid::Uuid::now_v7()),
        jobs,
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        sanvi_http::shutdown_signal().await;
        tracing::info!("shutdown signal received; draining worker");
        let _ = shutdown_tx.send(true);
    });

    tracing::info!(
        interval_ms = config.outbox.poll_interval_ms,
        "sanvi-worker starting",
    );

    let tenancy_shutdown = shutdown_rx.clone();
    let tenancy_stream = format!("{}.tenancy", config.outbox.stream);
    let tenancy_task = tokio::spawn(async move {
        tenancy_consumer
            .run_until_shutdown(
                &tenancy_stream,
                "tenancy",
                &tenancy_consumer_name,
                tenant_event_handler(invalidate),
                tenancy_shutdown,
            )
            .await;
    });

    let localization_shutdown = shutdown_rx.clone();
    let localization_stream = format!("{}.localization", config.outbox.stream);
    let localization_task = tokio::spawn(async move {
        localization_consumer
            .run_until_shutdown(
                &localization_stream,
                "localization",
                &localization_consumer_name,
                localization_event_handler(localization_service),
                localization_shutdown,
            )
            .await;
    });

    let theming_shutdown = shutdown_rx.clone();
    let theming_stream = format!("{}.theming", config.outbox.stream);
    let theming_task = tokio::spawn(async move {
        theming_consumer
            .run_until_shutdown(
                &theming_stream,
                "theming",
                &theming_consumer_name,
                theming_event_handler(theming_cache),
                theming_shutdown,
            )
            .await;
    });

    let domains_shutdown = shutdown_rx.clone();
    let domains_stream = format!("{}.domains", config.outbox.stream);
    let domains_cache = domains_cache.clone();
    let domains_task = tokio::spawn(async move {
        domains_consumer
            .run_until_shutdown(
                &domains_stream,
                "domains",
                &domains_consumer_name,
                domains_event_handler(domains_cache),
                domains_shutdown,
            )
            .await;
    });

    let access_shutdown = shutdown_rx.clone();
    let identity_stream = format!("{}.identity", config.outbox.stream);
    let access_task = tokio::spawn(async move {
        access_consumer
            .run_until_shutdown(
                &identity_stream,
                "access",
                &access_consumer_name,
                access_handler,
                access_shutdown,
            )
            .await;
    });

    let entitlement_shutdown = shutdown_rx.clone();
    let access_stream = format!("{}.access", config.outbox.stream);
    let entitlement_task = tokio::spawn(async move {
        entitlement_consumer
            .run_until_shutdown(
                &access_stream,
                "access-entitlements",
                &entitlement_consumer_name,
                entitlement_handler,
                entitlement_shutdown,
            )
            .await;
    });

    let billing_shutdown = shutdown_rx.clone();
    let billing_stream = format!("{}.billing", config.outbox.stream);
    let billing_task = tokio::spawn(async move {
        billing_consumer
            .run_until_shutdown(
                &billing_stream,
                "billing",
                &billing_consumer_name,
                billing_event_handler,
                billing_shutdown,
            )
            .await;
    });

    let scheduler_shutdown = shutdown_rx.clone();
    let scheduler_task = tokio::spawn(async move {
        scheduler.run_until_shutdown(scheduler_shutdown).await;
    });

    let dispatcher_task = tokio::spawn(async move {
        dispatcher
            .run_until_shutdown(
                Duration::from_millis(config.outbox.poll_interval_ms),
                shutdown_rx,
            )
            .await;
    });

    let _ = tokio::join!(
        dispatcher_task,
        tenancy_task,
        localization_task,
        theming_task,
        domains_task,
        access_task,
        entitlement_task,
        billing_task,
        scheduler_task
    );

    telemetry_guard.shutdown();
    ExitCode::SUCCESS
}

#[cfg(test)]
mod config_tests {
    use sanvi_kernel::IdsConfig;

    #[test]
    fn staging_requires_an_explicit_node_id() {
        let err = IdsConfig::default().resolve("staging", 2).unwrap_err();

        assert!(err.to_string().contains("staging"));
    }

    #[test]
    fn local_worker_uses_reserved_node_two_when_unset() {
        let node = IdsConfig::default().resolve("local", 2).unwrap();

        assert_eq!(node.get(), 2);
    }
}
