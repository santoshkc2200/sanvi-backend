//! `sanvi-worker`: PostgreSQL outbox consumers and durable recurring jobs.
//!
//! Committed outbox events are delivered from PostgreSQL with per-group leases,
//! durable inbox deduplication and replayable dead letters. Database effects can
//! share the inbox transaction; external effects must be idempotent.
//! Recurring schedules and failure budgets survive worker restarts in PostgreSQL.
//! Redis supports cache invalidation and quotas independently of job coordination.
//! SIGTERM/SIGINT stops polling; interrupted work remains available for replay.

use std::process::ExitCode;
mod management;
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
        HttpKetoClient, OutboxEventPublisher as AccessEventPublisher, RedisEntitlementCache,
        SqlxEntitlementRepository, SqlxFeatureRepository, SqlxQuotaUsageStore, SqlxRoleCatalog,
        SqlxRoleRepository, SystemClock as AccessSystemClock, sync::entitlement_sync_handler,
    },
    application::{EntitlementFeatureGate, EntitlementResolver, ExpireEntitlements},
    domain::{
        Clock, EntitlementCache, EntitlementRepository, EventPublisher, FeatureKey,
        PermissionStore, QuotaUsageStore, RoleRepository,
    },
};
use sanvi_advertising::{
    AudienceRepository, PlatformKey,
    adapters::{
        AdvertisingMetrics, OutboxEventPublisher as AdvertisingEventPublisher,
        SqlxAdvertisingConnectionRepository, SqlxCampaignRepository,
    },
    application::{
        ReconcileCampaign, ReconcileDueCampaigns, RetractWithdrawnConversions, SyncAudiences,
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
        IdGenerator as BillingIds, InvoiceRepository, PlanEntitlementRepository,
        PlanPriceRepository, PlanRepository, StripeClient, SubscriptionRepository,
        WebhookEventRepository,
    },
};
use sanvi_domains::{DomainsConfig, adapters::events::domains_event_handler};
use sanvi_kernel::TenantId as PrivacyTenantId;
use sanvi_kernel::notifications::{Audience, IntentArg};
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
    ConsumerMetrics, EventConsumer, FnJob, OutboxDispatcher, OutboxMetrics, RedisConfig, Scheduler,
    StreamName, connect_redis,
};
use sanvi_notifications::{
    CopyRenderer, EnqueueEmail, HttpNotificationGateway, NotificationDispatcher, NotificationError,
    NotificationGatewayPort, Recipient, RecipientDirectory, RenderedNotification,
    build_idempotency_key, secret_fingerprint,
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
        ConnectionLifecycleStore, EventPublisher as PaymentsEventPublisher,
        WebhookEventRepository as PaymentsWebhookEventRepository,
    },
};
use sanvi_persistence::{DatabaseConfig, build_pool};
use sanvi_privacy::{
    PrivacyConfig,
    adapters::{
        ArchivesDestination, BackupWindowDestination, CacheDestination, DisabledEnvelopeCrypto,
        LoggingSubProcessorNotifier, OutboxEventPublisher as PrivacyEvents, PrivacyContextProvider,
        SearchIndexDestination, Sha256Hasher, SqlxAppealRepository, SqlxAuthorizedAgentRepository,
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
        PropagateErasure, PropagateOptOuts, RetentionSweep,
    },
    domain::{
        AppealRepository, AuthorizedAgentRepository, BreachRepository, ConsentRepository,
        DestinationRetainedItem, DirectiveRepository, DirectiveResolver, DsrRepository,
        EnvelopeCrypto, EraseDestinationOutcome, ErasureDestination, ExportArtifactRepository,
        Hasher, JurisdictionRepository, LegalHoldRepository,
        Notifications as PrivacyNotificationsTrait, ParticipantRepository, PrivacyError,
        ProviderRegistry, RequestMetricsRepository, RetentionRuleRepository, SignalRepository,
        SubProcessorNotifier, SubProcessorRepository, SubjectKeyRepository, SubjectRef,
        SubjectRepository, VerificationRepository,
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

struct WorkerRecipientDirectory {
    users: Arc<dyn sanvi_identity::domain::UserRepository>,
    tx_manager: sanvi_persistence::TxManager,
    memberships: Arc<dyn sanvi_identity::domain::MembershipRepository>,
    roles: Arc<dyn sanvi_identity::domain::RoleCatalog>,
}

#[async_trait::async_trait]
impl RecipientDirectory for WorkerRecipientDirectory {
    async fn resolve(&self, audience: &Audience) -> Result<Vec<Recipient>, NotificationError> {
        match audience {
            Audience::User(id) => {
                let mut scope = self
                    .tx_manager
                    .begin_unscoped("notification_user", "notifications")
                    .await
                    .map_err(|err| NotificationError::Unavailable {
                        detail: err.to_string(),
                    })?;
                let user = self
                    .users
                    .find_by_id(&mut scope, *id)
                    .await
                    .map_err(|err| NotificationError::Unavailable {
                        detail: err.to_string(),
                    })?;
                scope
                    .commit()
                    .await
                    .map_err(|err| NotificationError::Unavailable {
                        detail: err.to_string(),
                    })?;
                Ok(user
                    .into_iter()
                    .filter(|user| user.email_verified())
                    .map(|user| Recipient {
                        key: id.to_string(),
                        email: user.primary_email().as_str().to_string(),
                    })
                    .collect())
            }
            Audience::Email(addr) => Ok(vec![Recipient {
                key: String::new(),
                email: addr.clone(),
            }]),
            Audience::TenantRoles { tenant_id, roles } => {
                let assignable = self
                    .roles
                    .assignable_roles(*tenant_id)
                    .await
                    .map_err(|err| NotificationError::Unavailable {
                        detail: err.to_string(),
                    })?;

                let matching_role_ids: std::collections::HashSet<_> = assignable
                    .into_iter()
                    .filter(|(_, key)| roles.contains(&key.as_str()))
                    .map(|(id, _)| id)
                    .collect();

                let mut scope = self
                    .tx_manager
                    .begin_for(tenant_id.as_i64())
                    .await
                    .map_err(|err| NotificationError::Unavailable {
                        detail: err.to_string(),
                    })?;

                let members = self
                    .memberships
                    .list_with_emails(&mut scope, *tenant_id, 200, None)
                    .await
                    .map_err(|err| NotificationError::Unavailable {
                        detail: err.to_string(),
                    })?;

                scope
                    .commit()
                    .await
                    .map_err(|err| NotificationError::Unavailable {
                        detail: err.to_string(),
                    })?;

                let mut recipients = Vec::new();
                for (membership, email) in members {
                    if membership.is_active()
                        && membership
                            .role_ids()
                            .iter()
                            .any(|id| matching_role_ids.contains(id))
                    {
                        recipients.push(Recipient {
                            key: membership.user_id().to_string(),
                            email: email.as_str().to_string(),
                        });
                    }
                }
                recipients.sort_by(|a, b| a.key.cmp(&b.key));
                recipients.dedup_by(|a, b| a.key == b.key);
                Ok(recipients)
            }
            Audience::Platform => {
                tracing::debug!("platform audience notifications not yet delivered");
                Ok(vec![])
            }
        }
    }
}

struct WorkerCopyRenderer {
    localizer: Arc<NotificationLocalizer>,
}

#[async_trait::async_trait]
impl CopyRenderer for WorkerCopyRenderer {
    async fn render(
        &self,
        audience: &Audience,
        subject_key: &str,
        body_key: &str,
        args: &[(String, IntentArg)],
    ) -> Result<RenderedNotification, NotificationError> {
        let context = match audience {
            Audience::TenantRoles { tenant_id, .. } => {
                self.localizer.tenant_locale(*tenant_id).await
            }
            _ => self.localizer.platform_locale().await,
        };

        let loc_args: Vec<(&str, LocalizationArg)> = args
            .iter()
            .map(|(k, v)| {
                let arg = match v {
                    IntentArg::Text(s) => LocalizationArg::Text(s.clone()),
                    IntentArg::Int(i) => LocalizationArg::Number(*i as f64),
                    IntentArg::Timestamp(dt) => LocalizationArg::DateTime(*dt),
                };
                (k.as_str(), arg)
            })
            .collect();

        let subject = self.localizer.render(&context, subject_key, &loc_args);
        let text_body = self.localizer.render(&context, body_key, &loc_args);
        let html_body = sanvi_localization::adapters::email_html::email_html(
            &context.locale,
            &subject,
            &text_body,
            None,
        );

        Ok(RenderedNotification {
            locale: context.locale,
            subject,
            text_body,
            html_body,
        })
    }
}

/// The worker's tenant-timezone source for budget periods (advertising
/// port): the tenant's locale settings from the localization context.
/// Advertising must not import another context's domain, so the port is
/// implemented here at the composition root. Unset settings resolve to
/// `"UTC"`, matching `TenantLocaleSettings::default_for`.
struct WorkerTenantTimezone;

#[async_trait::async_trait]
impl sanvi_advertising::domain::TenantTimezoneSource for WorkerTenantTimezone {
    async fn timezone(
        &self,
        scope: &mut sanvi_persistence::ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<String, sanvi_advertising::domain::AdvertisingError> {
        use sanvi_localization::domain::TenantLocaleRepository as _;
        let repo = sanvi_localization::adapters::SqlxTenantLocaleRepository::new();
        let settings = repo
            .get(scope, tenant_id)
            .await
            .map_err(|err| sanvi_advertising::domain::AdvertisingError::Storage(err.to_string()))?;
        Ok(settings
            .map(|s| s.timezone)
            .unwrap_or_else(|| "UTC".to_string()))
    }
}

/// The worker's per-tenant budget-guardrails entitlement check (advertising
/// port) through the access context's cached resolver — the same resolution
/// the HTTP feature gate performs, so the sweep and the routes agree on who
/// is rolled out. Every error fails closed (`false`): an uncertain gate
/// skips evaluation, never auto-pauses.
struct WorkerBudgetEntitlement {
    gate: Arc<sanvi_access::application::EntitlementFeatureGate>,
    feature: sanvi_access::domain::FeatureKey,
}

#[async_trait::async_trait]
impl sanvi_advertising::domain::BudgetGuardrailEntitlement for WorkerBudgetEntitlement {
    async fn is_enabled(
        &self,
        tenant_id: TenantId,
    ) -> Result<bool, sanvi_advertising::domain::AdvertisingError> {
        use sanvi_access::domain::FeatureGate as _;
        match self.gate.is_enabled(tenant_id, &self.feature).await {
            Ok(enabled) => Ok(enabled),
            Err(sanvi_access::domain::AccessError::FeatureNotFound { key }) => {
                tracing::warn!(
                    %tenant_id,
                    %key,
                    "budget guardrails feature missing from catalog; failing closed"
                );
                Ok(false)
            }
            Err(err) => Err(sanvi_advertising::domain::AdvertisingError::Storage(
                err.to_string(),
            )),
        }
    }
}

/// Customer-facing tenant labels for rollback notices (advertising port).
/// Falls back to the tenant id when the directory read fails rather than
/// dropping the notice.
struct WorkerTenantDirectory {
    tx_manager: sanvi_persistence::TxManager,
    tenants: Arc<dyn sanvi_tenancy::domain::TenantRepository>,
}

#[async_trait::async_trait]
impl sanvi_advertising::domain::TenantDirectory for WorkerTenantDirectory {
    async fn display_name(
        &self,
        tenant_id: TenantId,
    ) -> Result<String, sanvi_advertising::domain::AdvertisingError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("budget_rollback_directory", "tenancy")
            .await
            .map_err(|err| sanvi_advertising::domain::AdvertisingError::Storage(err.to_string()))?;
        let tenant = self
            .tenants
            .find_by_id(&mut scope, tenant_id)
            .await
            .map_err(|err| sanvi_advertising::domain::AdvertisingError::Storage(err.to_string()))?;
        scope
            .commit()
            .await
            .map_err(|err| sanvi_advertising::domain::AdvertisingError::Storage(err.to_string()))?;
        Ok(tenant
            .map(|t| t.display_name().to_string())
            .unwrap_or_else(|| tenant_id.to_string()))
    }
}

/// The worker's half of privacy Lane B.
///
/// `ProcessDsr` runs here, so export credentials are minted and mailed on this
/// side while DSR verification codes are minted in the API. Both payloads are
/// live credentials and never travel through the outbox. Neither the code, the
/// passphrase, the token nor the recipient is logged.
struct WorkerPrivacyNotifications {
    localizer: Arc<NotificationLocalizer>,
    gateway: Option<Arc<dyn NotificationGatewayPort>>,
}

#[async_trait::async_trait]
impl PrivacyNotificationsTrait for WorkerPrivacyNotifications {
    async fn send_verification_code(
        &self,
        to: &str,
        code: &str,
        request_id: sanvi_privacy::domain::DsrId,
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
        let html = sanvi_localization::adapters::email_html::email_html(
            &context.locale,
            &subject,
            &body,
            None,
        );

        let Some(gateway) = &self.gateway else {
            tracing::info!(
                request_id = %request_id,
                locale = %context.locale,
                notification = "verification",
                "notification service not configured; verification code not delivered"
            );
            return Ok(());
        };

        let key = build_idempotency_key(
            "privacy",
            "verification",
            &request_id.to_string(),
            &secret_fingerprint(code),
        );
        gateway
            .enqueue_email(EnqueueEmail {
                tenant_id: None,
                recipient: to,
                subject: &subject,
                text_body: &body,
                html_body: Some(&html),
                idempotency_key: &key,
            })
            .await
            .map_err(|err| sanvi_privacy::domain::PrivacyError::Upstream(err.to_string()))?;
        Ok(())
    }

    async fn send_export_credentials(
        &self,
        subject_ref: &sanvi_privacy::domain::SubjectRef,
        recipient: &str,
        token: &str,
        passphrase: &str,
        request_id: sanvi_privacy::domain::DsrId,
    ) -> Result<(), sanvi_privacy::domain::PrivacyError> {
        let context = match subject_ref.tenant_id {
            Some(tenant_id) => self.localizer.tenant_locale(tenant_id).await,
            None => self.localizer.platform_locale().await,
        };
        let request = request_id.to_string();
        let subject = self
            .localizer
            .render(&context, "emails.export_passphrase.subject", &[]);
        let body = self.localizer.render(
            &context,
            "emails.export_passphrase.body",
            &[
                ("request_id", LocalizationArg::from(request.as_str())),
                ("passphrase", LocalizationArg::from(passphrase)),
            ],
        );

        let Some(gateway) = &self.gateway else {
            tracing::info!(
                subject_ref = %subject_ref.canonical_key(),
                request_id = %request_id,
                locale = %context.locale,
                notification = "export_credentials",
                "notification service not configured; export credentials not delivered"
            );
            return Ok(());
        };

        let tenant = subject_ref.tenant_id.map(|id| id.to_string());
        let html = sanvi_localization::adapters::email_html::email_html(
            &context.locale,
            &subject,
            &body,
            None,
        );
        let key = build_idempotency_key("privacy", "export", &request, &secret_fingerprint(token));
        gateway
            .enqueue_email(EnqueueEmail {
                tenant_id: tenant.as_deref(),
                recipient,
                subject: &subject,
                text_body: &body,
                html_body: Some(&html),
                idempotency_key: &key,
            })
            .await
            .map_err(|err| sanvi_privacy::domain::PrivacyError::Upstream(err.to_string()))?;
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
    /// Pre-create current and following partitions for every registry
    /// parent (TASK-028: one job, one registry — advertising, audit, outbox
    /// and the webhook histories). This starts before any conversion
    /// endpoint exists: partition maintenance cannot be an afterthought on
    /// the first day of a new month.
    pub partition_maintenance_interval_secs: u64,
    /// Cross-tenant campaign reconciliation sweep (TASK-012, 10.3) — the
    /// nightly safety net alongside on-demand reconciliation. Never mutates
    /// a platform; only observes and records drift.
    pub advertising_reconciliation_interval_secs: u64,
    /// Conversion upload worker sweep (TASK-015, 10.6).
    pub advertising_conversion_upload_interval_secs: u64,
    /// Conversion retraction worker sweep (TASK-015, 10.6).
    pub advertising_conversion_retraction_interval_secs: u64,
    /// Audience sync worker sweep (TASK-015, 10.6).
    pub advertising_audience_sync_interval_secs: u64,
    /// Metrics ingestion sweep (TASK-016, 10.7). One schedule covers both
    /// "hourly for today" and the trailing backfill — see
    /// `RunMetricsIngestion::execute`'s doc comment for why.
    pub advertising_metrics_ingestion_interval_secs: u64,
    /// Scheduled OAuth token-refresh sweep (TASK-018, 10.9).
    pub advertising_token_refresh_interval_secs: u64,
    /// How long a connection may go unverified before that sweep picks it up.
    /// Distinct from the sweep's own cadence: the pass runs often, each
    /// connection is checked rarely.
    pub advertising_token_refresh_stale_after_secs: u64,
    /// Budget guardrails evaluation & spend anomaly sweep (TASK-017, 10.8).
    pub advertising_budget_guardrails_interval_secs: u64,
    /// Budget rollover sweep (TASK-017, 10.8).
    pub advertising_budget_rollover_interval_secs: u64,
    /// TASK-028 (FR-1130): the archival sweep — archive-action retention
    /// classes age out to cold storage, monthly steps per tenant.
    pub archival_sweep_interval_secs: u64,
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
            partition_maintenance_interval_secs: 86_400,
            advertising_reconciliation_interval_secs: 86_400,
            advertising_conversion_upload_interval_secs: 60,
            advertising_conversion_retraction_interval_secs: 60,
            advertising_audience_sync_interval_secs: 3600,
            advertising_metrics_ingestion_interval_secs: 3600,
            advertising_token_refresh_interval_secs: 3600,
            advertising_token_refresh_stale_after_secs: 21_600,
            advertising_budget_guardrails_interval_secs: 300,
            advertising_budget_rollover_interval_secs: 3600,
            archival_sweep_interval_secs: 86_400,
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

/// The subset of the api's `[features]` section this process acts on.
///
/// Same section the api reads, declared separately because the worker does not
/// depend on `sanvi-api` — exactly as [`OryConfig`] is. Serde ignores the keys
/// listed here that only gate api routes.
///
/// Unlike the api's struct, `Default` is written out rather than derived, so an
/// absent `[features]` section and an absent key mean the same thing. A derived
/// all-off default would silently stop every outbound notification whenever the
/// section was missing, which is the opposite of what a missing rollback switch
/// should do.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct FeaturesConfig {
    /// Gates the notification-service integration: the six Lane A consumer
    /// groups and the worker's Lane B export-credentials mail. Turning this
    /// off must stop *all* outbound mail from this process.
    #[serde(default = "default_true")]
    pub notifications: bool,
    /// Mirrors the api's `features.advertising` rollout flag (same
    /// `[features]` section, same default-off semantics). The budget
    /// guardrail evaluation, anomaly sweep, and rollover sweep below are the
    /// only jobs gated here: with the flag off the scheduler still runs, but
    /// every budget tick is a no-op — disabling the rollout stops alerts and
    /// destructive auto-pauses, which permission checks alone cannot do.
    #[serde(default)]
    pub advertising: bool,
    /// TASK-028 (FR-1129) `platform.archival`: gates the archival sweep
    /// job (archive-action retention classes aging out to cold storage).
    /// With the flag off the job is a no-op — same rollback semantics as
    /// `advertising`.
    #[serde(default)]
    pub archival: bool,
}

/// Why this process is not sending notifications, if it is not.
///
/// The two reasons are kept apart because an operator debugging silent mail
/// needs to know whether the rollback switch was thrown or the endpoint was
/// never configured.
fn notifications_disabled_reason(config: &WorkerConfig) -> Option<&'static str> {
    if !config.features.notifications {
        Some("features.notifications is off")
    } else if config.notification.base_url.is_empty() {
        Some("notification service base_url is empty")
    } else {
        None
    }
}

impl Default for FeaturesConfig {
    fn default() -> Self {
        Self {
            notifications: default_true(),
            advertising: false,
            archival: false,
        }
    }
}

const fn default_true() -> bool {
    true
}

/// The worker's pool is not the api's. `[database]` is the request-facing
/// pool (the api reads it directly); the worker applies this overlay on top
/// of it, so every process gets its own measured ceiling and its own
/// statement budget (TASK-021 11.2f). Every field is optional: absent keys
/// inherit the shared `[database]` value.
///
/// The statement budget is the one that *differs by shape*: the api's cap
/// exists so a client request fails inside its budget (FR-1110); worker jobs
/// have no client waiting and legitimately run longer statements than any
/// request (retention deletes, metrics aggregation over partitions), so the
/// worker's budget is wider. It is not uncapped: a batch statement that
/// cannot finish inside it needs chunking (TASK-028's archival work), not a
/// bigger cap.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct WorkerPoolConfig {
    pub max_connections: Option<u32>,
    pub min_connections: Option<u32>,
    pub statement_timeout_ms: Option<u64>,
    /// Distinct from the api's `application_name`, so `pg_stat_activity`
    /// attributes connections — and pool demand — to the process holding
    /// them.
    pub application_name: Option<String>,
}

impl WorkerPoolConfig {
    /// Merge the overlay onto the shared `[database]` values. Runs before
    /// `validate`, so the merged pair is what is checked and what
    /// `build_pool` sees.
    fn apply(&self, database: &mut DatabaseConfig) {
        if let Some(max) = self.max_connections {
            database.max_connections = max;
        }
        if let Some(min) = self.min_connections {
            database.min_connections = min;
        }
        if let Some(timeout) = self.statement_timeout_ms {
            database.statement_timeout_ms = timeout;
        }
        if let Some(name) = &self.application_name {
            database.application_name = name.clone();
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct WorkerConfig {
    #[serde(default)]
    pub worker_management: management::ManagementConfig,
    #[serde(default)]
    pub media: sanvi_media::MediaConfig,
    #[serde(default)]
    pub platform: sanvi_http::config::PlatformApiConfig,
    #[serde(default)]
    pub ids: IdsConfig,
    pub database: DatabaseConfig,
    #[serde(default)]
    pub worker_pool: WorkerPoolConfig,
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
    /// TASK-028 (FR-1129): the `[archival]` section (cold volume root).
    #[serde(default)]
    pub archival: sanvi_archival::config::ArchivalConfig,
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
    #[serde(default)]
    pub notification: sanvi_notifications::NotificationConfig,
    #[serde(default)]
    pub features: FeaturesConfig,
}

impl WorkerConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.worker_management
            .host
            .parse::<std::net::IpAddr>()
            .context("worker_management.host must be an IP address")?;
        anyhow::ensure!(
            self.worker_management.port > 0,
            "worker_management.port must be nonzero"
        );
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
        self.notification.validate()?;
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
    let mut config: WorkerConfig = Figment::new()
        .merge(Toml::file("configs/base.toml"))
        .merge(Toml::file(format!("configs/{environment}.toml")))
        .merge(Env::prefixed("SANVI__").split("__"))
        .extract()
        .context("failed to load configuration")?;
    // The worker's own pool ceiling and statement budget ride on top of the
    // shared `[database]` values (TASK-021 11.2f); validation and
    // `build_pool` see the merged pair.
    config.worker_pool.apply(&mut config.database);
    config.validate()?;
    config.ids.resolve(&config.telemetry.environment, 2)?;
    Ok(config)
}

/// TASK-029 (FR-1134): the ad-platform suppression-list destination. After
/// an erasure the subject's directives are superseded, so the phase-10
/// retraction sweep finds every already-uploaded conversion event and
/// retracts it at the platform; what a platform cannot retract is
/// documented in the ledger with its own reason rather than silently
/// counted as done. Lives in the composition root because the privacy
/// context never imports the advertising context.
struct AdSuppressionDestination {
    sweep: Arc<RetractWithdrawnConversions>,
}

#[async_trait::async_trait]
impl ErasureDestination for AdSuppressionDestination {
    fn name(&self) -> &'static str {
        "ad_suppression"
    }

    fn description(&self) -> String {
        "ad-platform suppression lists: uploaded conversion events are retracted at the platform; capture denies any new upload once the directives are superseded".to_string()
    }

    async fn propagate_erasure(
        &self,
        _subject: &SubjectRef,
        _tenant_id: Option<PrivacyTenantId>,
    ) -> Result<EraseDestinationOutcome, PrivacyError> {
        let report = self
            .sweep
            .execute(200)
            .await
            .map_err(|err| PrivacyError::Upstream(err.to_string()))?;
        let retained = if report.unpropagated > 0 {
            vec![DestinationRetainedItem {
                detail: format!(
                    "{} uploaded event(s) their platform cannot retract (capability unsupported by the platform, not by us)",
                    report.unpropagated
                ),
                reason:
                    "documented with the platform's own refusal reason in the event's upload states"
                        .to_string(),
                retains_until: None,
            }]
        } else {
            Vec::new()
        };
        Ok(EraseDestinationOutcome {
            purged: report.retracted,
            retained,
        })
    }
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

    // TASK-021 step 0: pool gauges plus the 1 Hz acquire probe, so pool
    // saturation is measured (step 5 sizes the pools from these numbers).
    let pool_metrics = std::sync::Arc::new(sanvi_persistence::PoolMetrics::new(
        metrics.prometheus_registry(),
    ));
    pool_metrics.spawn_sampler(pool.clone());

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

    let dispatcher = OutboxDispatcher::postgres(
        pool.clone(),
        StreamName(config.outbox.stream.clone()),
        config.outbox.batch_size,
        config.outbox.max_attempts,
    )
    .with_metrics(outbox_metrics.clone());

    // --- tenancy consumer: keeps resolution caches coherent -----------------
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let quota_runtime = sanvi_http::quota_middleware::QuotaRuntime {
        service: Arc::new(sanvi_http::quotas::QuotaService::new(
            tx_manager.clone(),
            Arc::new(sanvi_http::quotas::PgQuotaPlanProvider::new(
                tx_manager.clone(),
            )),
            Some(redis.clone()),
        )),
        enabled: config.platform.quotas,
    };
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
    let tenancy_consumer = EventConsumer::postgres(tx_manager.clone());
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
    let localization_consumer = EventConsumer::postgres(tx_manager.clone());
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
    // TASK-022 (FR-1111): the worker invalidates the storefront SSR cache on
    // the theme-publish / content-change events. Redis-backed: the deletions
    // reach every api replica inside the documented ~2 s window (outbox poll
    // 1 s + stream delivery).
    let ssr_cache: Arc<sanvi_cache::SsrCache> = Arc::new(sanvi_cache::SsrCache::new(
        Some(redis.clone()),
        Some(sanvi_cache::SsrCacheMetrics::new(
            metrics.prometheus_registry(),
        )),
    ));
    let theming_consumer = EventConsumer::postgres(tx_manager.clone());
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
    let domains_consumer = EventConsumer::postgres(tx_manager.clone());
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
    let access_projection: Arc<dyn sanvi_access::domain::AccessProjection> =
        Arc::new(sanvi_access::adapters::SqlxAccessProjection::new(
            access_tx.clone(),
            access_roles.clone(),
            Arc::new(sanvi_identity::adapters::repo::SqlxMembershipRepository::new()),
            access_store.clone(),
        ));
    let access_consumer = EventConsumer::postgres(tx_manager.clone());
    let access_consumer_metrics = Arc::new(ConsumerMetrics::new("access"));
    if let Err(err) = access_consumer_metrics.register(registry) {
        eprintln!("failed to register access consumer metrics: {err}");
        return ExitCode::from(1);
    }
    let access_consumer = access_consumer.with_metrics(access_consumer_metrics);
    let access_consumer_name = format!("access-{}", uuid::Uuid::now_v7());
    let decision_cache: Arc<dyn sanvi_access::domain::DecisionCache> = Arc::new(
        sanvi_access::adapters::RedisDecisionCache::new(redis.clone()),
    );
    let access_handler = sanvi_access::adapters::sync::access_sync_handler(
        access_projection.clone(),
        Some(decision_cache.clone()),
    );

    // --- access events consumer: entitlement cache + role authoring sync ----
    let entitlements: Arc<dyn EntitlementRepository> = Arc::new(SqlxEntitlementRepository::new());
    let quota: Arc<dyn QuotaUsageStore> = Arc::new(SqlxQuotaUsageStore::new());
    let access_events: Arc<dyn EventPublisher> = Arc::new(AccessEventPublisher::new(ids.clone()));
    let entitlement_cache: Arc<dyn EntitlementCache> = Arc::new(RedisEntitlementCache::new(
        redis.clone(),
        config.access.entitlement_cache_ttl_secs,
    ));

    // Budget-guardrails rollout gate: the same cached entitlement resolution
    // the HTTP feature gate performs, so the sweep and the routes agree on
    // which tenants are rolled out. Built here (not in the advertising
    // context) because advertising must not import access's domain.
    let budget_guardrails_feature: FeatureKey =
        match FeatureKey::parse("advertising.budget_guardrails") {
            Ok(key) => key,
            Err(err) => {
                eprintln!("invalid budget guardrails feature key: {err}");
                return ExitCode::from(2);
            }
        };
    let budget_entitlement_gate = Arc::new(EntitlementFeatureGate::new(
        Arc::new(EntitlementResolver::new(
            access_tx.clone(),
            Arc::new(SqlxFeatureRepository::new()),
            entitlements.clone(),
            entitlement_cache.clone(),
            Arc::new(sanvi_cache::SingleFlight::new()),
            Arc::new(sanvi_cache::CacheMetrics::new(registry)),
            Arc::new(AccessSystemClock),
            config.access.entitlement_cache_ttl_secs,
        )),
        access_tx.clone(),
        quota.clone(),
        access_events.clone(),
        Arc::new(AccessSystemClock),
    ));

    let entitlement_consumer = EventConsumer::postgres(tx_manager.clone());
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
        Some(access_projection),
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
            config.billing.stripe_api_key.expose_secret().clone(),
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
                    config.payments.connect_api_key.expose_secret().clone(),
                    config.payments.stripe_api_version.clone(),
                    config
                        .payments
                        .connect_webhook_secret
                        .expose_secret()
                        .clone(),
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
            payments_events.clone(),
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
            chrono::Duration::seconds(
                config.scheduler.payments_connection_sync_interval_secs as i64,
            ),
        )
        .with_metrics(payments_metrics.clone()),
    );

    // --- billing events consumer: entitlement cache invalidation ------------
    let billing_consumer = EventConsumer::postgres(tx_manager.clone());
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
    let (domains_registrar, domains_registrar_mode) =
        sanvi_domains::adapters::build_domain_registrar(&config.domains)
            .expect("domains registrar must initialize cleanly");
    let (domains_ca, domains_ca_mode) =
        sanvi_domains::adapters::build_certificate_authority(&config.domains, ids.clone())
            .expect("domains certificate authority must initialize cleanly");
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
    let domains_metrics = std::sync::Arc::new(sanvi_domains::adapters::DomainsMetrics::new(
        metrics.prometheus_registry(),
    ));

    // The worker's charger is the same charger the API uses.
    let (domains_charger, domains_charger_mode) =
        sanvi_domains::adapters::build_purchase_charger(&config.domains)
            .expect("domains purchase charger must initialize cleanly");
    tracing::info!(
        registrar_mode = %domains_registrar_mode,
        ca_mode = %domains_ca_mode,
        charger_mode = %domains_charger_mode,
        "domains capabilities initialized"
    );

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
    if let Err(err) = sanvi_privacy::adapters::erasure_ledger::queue_restore_replay(
        &privacy_tx,
        &config.privacy,
        ids.clone(),
        &sanvi_audit::adapters::PrivacyAuditWriterAdapter::new(Arc::new(SqlxAuditWriter::new(
            ids.clone(),
        ))),
    )
    .await
    {
        eprintln!("privacy recovery replay failed: {err}");
        return ExitCode::from(1);
    }

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
    // One decision for the whole process: the Lane A consumer groups and the
    // Lane B export-credentials adapter both read it, so
    // `features.notifications = false` stops *all* outbound mail from the
    // worker rather than half of it.
    let notifications_enabled = match notifications_disabled_reason(&config) {
        Some(reason) => {
            tracing::info!(reason, "worker notification delivery disabled");
            false
        }
        None => true,
    };

    // Delivery for privacy Lane B (export credentials). `None` leaves the
    // adapter logging instead of sending. Sends count against the tenant's
    // abuse window (TASK-024), shared with the API's gateway via Redis.
    let privacy_notification_gateway: Option<std::sync::Arc<dyn NotificationGatewayPort>> =
        if notifications_enabled {
            Some(std::sync::Arc::new(
                HttpNotificationGateway::new(config.notification.clone())
                    .with_abuse_counter(
                        std::sync::Arc::new(sanvi_http::abuse::AbuseCounter::new(Some(
                            redis.clone(),
                        ))),
                        config.notification.send_quota_limit,
                        config.notification.send_quota_window_secs,
                    )
                    .with_commercial_quota(quota_runtime.clone()),
            ))
        } else {
            None
        };
    let privacy_events = std::sync::Arc::new(PrivacyEvents::new(ids.clone()));
    let privacy_notifications: std::sync::Arc<dyn PrivacyNotificationsTrait> =
        std::sync::Arc::new(WorkerPrivacyNotifications {
            localizer: notification_localizer.clone(),
            gateway: privacy_notification_gateway.clone(),
        });
    let privacy_clock = std::sync::Arc::new(PrivacySystemClock);
    let privacy_ids: std::sync::Arc<dyn sanvi_privacy::domain::IdGenerator> =
        std::sync::Arc::new(PrivacySystemIdGenerator::new(ids.clone()));
    let privacy_hasher: std::sync::Arc<dyn Hasher> =
        std::sync::Arc::new(Sha256Hasher::new(if config.privacy.kek.is_empty() {
            "dev-pepper-insecure".to_string()
        } else {
            config.privacy.kek.expose_secret().clone()
        }));
    let privacy_crypto: std::sync::Arc<dyn EnvelopeCrypto> = if config.privacy.encryption_enabled()
    {
        let kek: [u8; 32] = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            config.privacy.kek.expose_secret(),
        )
        .expect("validated by PrivacyConfig")
        .try_into()
        .expect("validated length");
        std::sync::Arc::new(
            SqlxEnvelopeCrypto::new(
                privacy_tx.clone(),
                privacy_keys.clone(),
                kek,
                config.privacy.kek_id.clone(),
            )
            .with_erasure_ledger(config.privacy.erasure_ledger_dir.clone()),
        )
    } else {
        std::sync::Arc::new(DisabledEnvelopeCrypto)
    };
    let privacy_notifier: std::sync::Arc<dyn SubProcessorNotifier> =
        std::sync::Arc::new(LoggingSubProcessorNotifier);

    // The provider registry includes every context the startup check enforces.
    // The worker rebuilds it (providers are stateless ZSTs + repos).
    let privacy_providers: ProviderRegistry = vec![
        std::sync::Arc::new(sanvi_system::adapters::SystemPrivacyProvider::new()),
        std::sync::Arc::new(sanvi_product_access::adapters::ProductAccessPrivacyProvider::new()),
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
        std::sync::Arc::new(
            sanvi_advertising::adapters::AdvertisingPrivacyProvider::new(privacy_tx.clone()),
        ),
        std::sync::Arc::new(sanvi_media::adapters::MediaPrivacyProvider::with_storage(
            privacy_tx.clone(),
            config.media.clone(),
        )),
        std::sync::Arc::new(sanvi_notifications::NotificationsPrivacyProvider::new()),
    ];
    // TASK-029: the worker drives the erasure saga, so its registry copy
    // is held to the same completeness bar as the API's: a context that
    // quietly never joined fails at boot, not in an audit.
    {
        let registered: std::collections::BTreeSet<&str> =
            privacy_providers.iter().map(|p| p.context_name()).collect();
        let expected: std::collections::BTreeSet<&str> = sanvi_privacy::domain::CONTEXT_NAMES
            .iter()
            .copied()
            .collect();
        assert_eq!(
            registered, expected,
            "worker privacy provider registry must contain every context exactly once"
        );
    }

    let dsr_process = std::sync::Arc::new(ProcessDsrBatch::new(
        privacy_tx.clone(),
        privacy_requests.clone(),
        privacy_consents.clone(),
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
    let audit_writer: Arc<dyn sanvi_audit::domain::AuditWriter> =
        Arc::new(SqlxAuditWriter::new(ids.clone()));
    let verify_audit = Arc::new(VerifyAuditChain::new(
        tx_manager.clone(),
        audit_reader,
        config.audit.verify_batch_size,
    ));

    // TASK-012 (10.3): the nightly reconciliation sweep. When `real_platform_adapters`
    // is false (default), the fake adapter is used for every platform; when
    // true, real Google Ads / Meta adapters are registered where configured,
    // falling back to the fake adapter per platform if credentials are missing
    // or invalid.
    let mut advertising_token_providers: std::collections::HashMap<
        PlatformKey,
        Arc<dyn sanvi_advertising::domain::ConnectionAccessTokenProvider>,
    > = std::collections::HashMap::new();
    match config.advertising.token_kek_bytes() {
        Ok(token_kek) => {
            let token_vault = Arc::new(sanvi_advertising::domain::TokenVault::new(
                Arc::new(sanvi_advertising::adapters::SqlxTenantEnvelopeCrypto::new(
                    token_kek,
                    config.advertising.token_kek_id.clone(),
                )),
                Arc::new(sanvi_advertising::adapters::SqlxConnectionTokenRepository::new()),
            ));
            advertising_token_providers = sanvi_advertising::adapters::build_access_token_providers(
                &config.advertising,
                tx_manager.clone(),
                token_vault,
            );
        }
        Err(error) => {
            // Without the KEK nothing can decrypt a stored refresh token; the
            // sweep still runs and reports every connection as skipped.
            tracing::warn!(%error, "advertising token refresh: token vault unavailable");
        }
    }

    let advertising_campaigns: Arc<dyn AdvertisingCampaignRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let advertising_events: Arc<dyn AdvertisingEvents> =
        Arc::new(AdvertisingEventPublisher::new(ids.clone()));
    let advertising_ids: Arc<dyn sanvi_advertising::domain::IdGenerator> = Arc::new(
        sanvi_advertising::adapters::SystemIdGenerator::new(ids.clone()),
    );
    let (ad_platform_registry, ad_platform_mode) =
        match sanvi_advertising::adapters::build_platform_registry(
            &config.advertising,
            &advertising_token_providers,
        ) {
            Ok(res) => res,
            Err(err) => {
                tracing::error!(%err, "failed to initialize advertising platform registry");
                return std::process::ExitCode::FAILURE;
            }
        };
    let advertising_platforms = Arc::new(sanvi_advertising::adapters::rate_limited_registry(
        &ad_platform_registry,
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
        advertising_events.clone(),
        advertising_ids.clone(),
    ));
    let advertising_connections: Arc<dyn AdvertisingConnectionRepository> =
        Arc::new(SqlxAdvertisingConnectionRepository::new());
    let advertising_reconciliation_sweep = Arc::new(ReconcileDueCampaigns::new(
        tx_manager.clone(),
        advertising_campaigns.clone(),
        advertising_connections.clone(),
        advertising_platforms.clone(),
        advertising_reconcile_campaign,
    ));
    let (advertising_conversion_sinks, conversion_sink_mode) =
        match sanvi_advertising::adapters::build_conversion_sinks(
            &config.advertising,
            &advertising_token_providers,
        ) {
            Ok(res) => res,
            Err(err) => {
                tracing::error!(%err, "failed to initialize advertising conversion sinks");
                return std::process::ExitCode::FAILURE;
            }
        };
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
        advertising_conversion_events.clone(),
        advertising_connections.clone(),
        privacy_directive_resolver.clone(),
        advertising_conversion_sinks,
        advertising_metrics.clone(),
    ));

    // TASK-029 (FR-1134): after the saga erases a subject, propagation
    // reaches the destinations outside the provider registry and ledgers
    // each outcome. The job runs inside the dsr_advance pass (below), so a
    // completed erasure is never the last word until every destination is
    // purged or documented.
    let erasure_destinations: Vec<Arc<dyn ErasureDestination>> = vec![
        Arc::new(ArchivesDestination::new(
            privacy_tx.clone(),
            privacy_rules.clone(),
            privacy_clock.clone(),
        )),
        Arc::new(BackupWindowDestination {
            backup_retention_days: config.privacy.backup_retention_days,
            clock: privacy_clock.clone(),
        }),
        Arc::new(CacheDestination {
            ssr: ssr_cache.clone(),
        }),
        Arc::new(SearchIndexDestination),
        Arc::new(AdSuppressionDestination {
            sweep: advertising_conversion_retraction_sweep.clone(),
        }),
    ];
    let erasure_propagation = Arc::new(PropagateErasure::new(
        privacy_tx.clone(),
        erasure_destinations,
    ));

    let (advertising_audience_sinks, audience_sink_mode) =
        match sanvi_advertising::adapters::build_audience_sinks(
            &config.advertising,
            &advertising_token_providers,
        ) {
            Ok(res) => res,
            Err(err) => {
                tracing::error!(%err, "failed to initialize advertising audience sinks");
                return std::process::ExitCode::FAILURE;
            }
        };
    tracing::info!(
        platform_mode = %ad_platform_mode,
        conversion_sink_mode = %conversion_sink_mode,
        audience_sink_mode = %audience_sink_mode,
        "advertising capabilities initialized"
    );
    let advertising_audiences: Arc<dyn AudienceRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let advertising_audience_sync_sweep = Arc::new(SyncAudiences::new(
        tx_manager.clone(),
        advertising_audiences,
        advertising_conversion_events.clone(),
        advertising_connections.clone(),
        privacy_directive_resolver,
        advertising_audience_sinks,
        advertising_metrics.clone(),
    ));

    // TASK-016 (10.7): metrics ingestion, revenue join & the ROAS query
    // layer's ingestion sweep.
    let advertising_metrics_repo: Arc<dyn sanvi_advertising::domain::MetricsRepository> =
        Arc::new(sanvi_advertising::adapters::SqlxMetricsRepository::new());
    let advertising_ingest_metrics = Arc::new(
        sanvi_advertising::application::IngestConnectionMetrics::new(
            tx_manager.clone(),
            advertising_platforms.clone(),
            advertising_campaigns.clone(),
            advertising_metrics_repo.clone(),
        ),
    );
    let advertising_revenue_join =
        Arc::new(sanvi_advertising::application::JoinCampaignRevenue::new(
            tx_manager.clone(),
            advertising_campaigns.clone(),
            advertising_connections.clone(),
            advertising_conversion_events,
            advertising_metrics_repo.clone(),
        ));
    let advertising_metrics_ingestion_sweep =
        Arc::new(sanvi_advertising::application::RunMetricsIngestion::new(
            tx_manager.clone(),
            advertising_connections.clone(),
            advertising_ingest_metrics,
            advertising_revenue_join,
            advertising_metrics.clone(),
            config.advertising.metrics_ingestion_backfill_days,
        ));
    let advertising_token_refresh_sweep =
        Arc::new(sanvi_advertising::application::RunTokenRefresh::new(
            tx_manager.clone(),
            advertising_connections.clone(),
            advertising_token_providers,
            advertising_metrics.clone(),
            config.scheduler.advertising_token_refresh_stale_after_secs as i64,
        ));

    // TASK-017 (10.8): budget guardrails, spend alerts, threshold evaluator & rollover sweep.
    let advertising_budget_caps: Arc<dyn sanvi_advertising::domain::BudgetCapRepository> =
        Arc::new(sanvi_advertising::adapters::SqlxBudgetCapRepository::new());
    let advertising_budget_alerts: Arc<dyn sanvi_advertising::domain::BudgetAlertRepository> =
        Arc::new(sanvi_advertising::adapters::SqlxBudgetAlertRepository::new());
    let advertising_fx_rates: Arc<dyn sanvi_advertising::domain::FxRatePort> =
        sanvi_advertising::adapters::build_fx_rates(&config.advertising, tx_manager.clone());
    let advertising_timezones: Arc<dyn sanvi_advertising::domain::TenantTimezoneSource> =
        Arc::new(WorkerTenantTimezone);
    let advertising_spend_status = Arc::new(sanvi_advertising::application::GetSpendStatus::new(
        tx_manager.clone(),
        advertising_metrics_repo.clone(),
        advertising_budget_caps.clone(),
        advertising_campaigns.clone(),
        advertising_connections.clone(),
        advertising_fx_rates,
        advertising_timezones.clone(),
        config.advertising.metrics_restatement_days,
    ));
    let advertising_pause_campaign = Arc::new(sanvi_advertising::application::PauseCampaign::new(
        tx_manager.clone(),
        advertising_campaigns.clone(),
        advertising_connections.clone(),
        advertising_events.clone(),
        advertising_platforms.clone(),
        advertising_ids.clone(),
    ));
    // No `ResumeCampaign` here: budget guardrails never auto-resume (FR-1013),
    // so the rollover sweep has nothing to resume with.
    let advertising_threshold_evaluator =
        Arc::new(sanvi_advertising::application::ThresholdEvaluator::new(
            tx_manager.clone(),
            advertising_budget_caps.clone(),
            advertising_budget_alerts.clone(),
            advertising_campaigns.clone(),
            advertising_pause_campaign,
            advertising_spend_status,
            advertising_events.clone(),
            advertising_ids.clone(),
            advertising_metrics.clone(),
        ));
    let advertising_spend_anomaly_detector =
        Arc::new(sanvi_advertising::application::SpendAnomalyDetector::new(
            tx_manager.clone(),
            advertising_budget_alerts,
            advertising_metrics_repo,
            advertising_campaigns.clone(),
            advertising_events.clone(),
            advertising_ids,
            config.advertising.metrics_restatement_days,
        ));
    let advertising_rollover_sweep =
        Arc::new(sanvi_advertising::application::BudgetRolloverSweep::new(
            tx_manager.clone(),
            advertising_budget_caps,
            advertising_timezones,
        ));
    // Per-tenant rollout gate + rollback notices (TASK-017 follow-up): route
    // gating cannot protect background jobs, so the sweep partitions caps by
    // entitlement itself and emails every tenant whose retained caps stopped
    // being evaluated.
    let advertising_enforcement_gate =
        Arc::new(sanvi_advertising::application::EnforcementGate::new(
            tx_manager.clone(),
            Arc::new(WorkerBudgetEntitlement {
                gate: budget_entitlement_gate,
                feature: budget_guardrails_feature,
            }),
            Arc::new(WorkerTenantDirectory {
                tx_manager: tx_manager.clone(),
                tenants: tenants.clone(),
            }),
            advertising_events.clone(),
            // Durable suspension-notice dedupe: one notice per
            // enabled→suspended transition, surviving restarts and shared
            // across replicas.
            Arc::new(sanvi_advertising::adapters::SqlxGuardrailSuspensionLedger::new()),
        ));

    // TASK-028 (FR-1129/FR-1130): the archival sweep's cold storage.
    // Only when the feature is on AND the envelope KEK is configured —
    // an archive that cannot be decrypted is deletion with extra steps.
    let archival_storage: Option<Arc<sanvi_archival::storage::SealedStorage>> =
        if config.features.archival && config.privacy.encryption_enabled() {
            let kek: [u8; 32] = base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                config.privacy.kek.expose_secret(),
            )
            .expect("validated by PrivacyConfig")
            .try_into()
            .expect("validated length");
            Some(Arc::new(sanvi_archival::storage::SealedStorage::new(
                Arc::new(sanvi_archival::storage::LocalEncryptedVolume::new(
                    &config.archival.volume,
                )),
                Arc::new(
                    sanvi_privacy::adapters::archival_cipher::EnvelopeArchiveCipher::new(
                        privacy_crypto.clone(),
                        &kek,
                    ),
                ),
            )))
        } else {
            tracing::warn!(
                enabled = config.features.archival,
                "platform.archival sweep disabled (feature flag or KEK)"
            );
            None
        };

    let jobs: Vec<Arc<dyn sanvi_messaging::Job>> = vec![
        Arc::new(FnJob::new(
            "payments.refund_audits",
            Duration::from_secs(5),
            {
                let drain = Arc::new(sanvi_payments::application::DrainRefundAudits::new(
                    payments_tx.clone(),
                    Arc::new(sanvi_payments::adapters::SqlxRefundRepository::new()),
                    audit_writer.clone(),
                ));
                move || {
                    let drain = drain.clone();
                    Box::pin(async move {
                        drain.execute(100).await?;
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "advertising.budget_guardrails",
            Duration::from_secs(config.scheduler.advertising_budget_guardrails_interval_secs),
            {
                let evaluator = advertising_threshold_evaluator.clone();
                let anomaly_detector = advertising_spend_anomaly_detector.clone();
                let gate = advertising_enforcement_gate.clone();
                let enabled = config.features.advertising;
                move || {
                    let evaluator = evaluator.clone();
                    let anomaly_detector = anomaly_detector.clone();
                    let gate = gate.clone();
                    Box::pin(async move {
                        // Read the caps in a short transaction that commits before
                        // evaluation starts: each cap opens its own tenant-scoped
                        // transaction, so holding one here would keep a platform
                        // transaction open across every nested acquisition.
                        let caps = evaluator
                            .list_active_caps()
                            .await
                            .map_err(|e| anyhow::anyhow!(e))?;
                        use sanvi_advertising::application::SuspensionReason;

                        // Global rollback: no evaluation at all. Tenants whose
                        // caps were previously evaluated get the suspension
                        // notice (the ledger suppresses anyone never observed
                        // serving — a fresh deploy with the flag off is not a
                        // rollback); the rollback path includes it by spec.
                        if !enabled {
                            let mut tenants: Vec<_> = caps.iter().map(|c| c.tenant_id).collect();
                            tenants.sort_unstable();
                            tenants.dedup();
                            gate.notify_suspended(&tenants, SuspensionReason::FeatureDisabled)
                                .await;
                            return Ok(());
                        }

                        // Per-tenant rollout: route gating cannot protect
                        // background jobs, so non-entitled tenants are
                        // partitioned out here — their caps are never
                        // evaluated and never pause — and notified.
                        let (evaluable, suspended) = gate.partition(&caps).await;
                        gate.notify_suspended(&suspended, SuspensionReason::EntitlementOff)
                            .await;
                        if let Err(e) = evaluator.evaluate_caps(&evaluable).await {
                            tracing::error!(error = %e, "Budget guardrail sweep failed");
                        }

                        // Spend anomaly detection runs for every entitled
                        // tenant that has an active cap, reusing the cap
                        // listing rather than re-querying.
                        let mut tenants: Vec<_> = evaluable.iter().map(|c| c.tenant_id).collect();
                        tenants.sort_unstable();
                        tenants.dedup();
                        for tenant_id in tenants {
                            if let Err(e) = anomaly_detector
                                .detect_anomalies_for_tenant(tenant_id)
                                .await
                            {
                                tracing::warn!(
                                    %tenant_id,
                                    error = %e,
                                    "Spend anomaly detection failed for tenant; continuing"
                                );
                            }
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "advertising.budget_rollover",
            Duration::from_secs(config.scheduler.advertising_budget_rollover_interval_secs),
            {
                let rollover = advertising_rollover_sweep.clone();
                let enabled = config.features.advertising;
                move || {
                    let rollover = rollover.clone();
                    Box::pin(async move {
                        if !enabled {
                            return Ok(());
                        }
                        // Each sweep is a no-op unless the period actually rolled
                        // over since the last tick; the job interval is far shorter
                        // than either period.
                        for period in [
                            sanvi_advertising::domain::BudgetPeriod::Daily,
                            sanvi_advertising::domain::BudgetPeriod::Monthly,
                        ] {
                            if let Err(e) = rollover.sweep_all(period).await {
                                tracing::error!(
                                    period = period.as_str(),
                                    error = %e,
                                    "Budget rollover sweep failed"
                                );
                            }
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "platform.partition_maintenance",
            Duration::from_secs(config.scheduler.partition_maintenance_interval_secs),
            {
                let pool = pool.clone();
                move || {
                    let pool = pool.clone();
                    Box::pin(async move {
                        // TASK-028 (FR-1128): one job walks one registry —
                        // every partitioned parent in
                        // platform.partitioned_tables gets its current and
                        // pre-created following months, then the run is
                        // recorded as a heartbeat and the readiness probe
                        // (catalog-only, so it also catches this job being
                        // disabled) is evaluated: from the 25th on, a
                        // missing next-month child pages.
                        sqlx::query("SELECT platform.ensure_monthly_partitions(current_date)")
                            .execute(&pool)
                            .await?;
                        let breaches: Vec<(String, String, String)> = sqlx::query_as(
                            "SELECT schema_name, table_name, missing_child
                             FROM platform.partition_readiness_breaches(current_date)",
                        )
                        .fetch_all(&pool)
                        .await?;
                        if !breaches.is_empty() {
                            tracing::error!(
                                alert = "PartitionMaintenanceFailed",
                                breaches = ?breaches,
                                "next month's partition is missing for a registry parent (FR-1128)"
                            );
                        }
                        sqlx::query(
                            "INSERT INTO platform.partition_maintenance_log (day, ran_at, ok)
                             VALUES (current_date, now(), true)
                             ON CONFLICT (day) DO UPDATE
                               SET ran_at = excluded.ran_at, ok = true, detail = NULL",
                        )
                        .execute(&pool)
                        .await?;
                        // TASK-018 retention enforcement: the
                        // privacy.retention_rules registry is the single
                        // source of truth for both the cutoff *and* the
                        // action. The conversion-event class is registered
                        // `anonymise`: aged conversion partitions get their
                        // personal-data columns stripped (consent snapshot
                        // retained as proof) and are kept — only an explicit
                        // `delete` action authorizes a drop. A missing rule
                        // skips that class this pass — loudly, never
                        // silently.
                        let rules: Vec<(String, String, i64)> = sqlx::query_as(
                            "SELECT data_class, action,
                                    (EXTRACT(EPOCH FROM period) / 86400)::bigint AS days
                             FROM privacy.retention_rules
                             WHERE data_class IN ('advertising_conversion_event', 'advertising_metric')",
                        )
                        .fetch_all(&pool)
                        .await?;
                        let rules = rules
                            .into_iter()
                            .map(|(class, action, days)| (class, (action, days)))
                            .collect::<std::collections::HashMap<String, (String, i64)>>();

                        let mut conversion_cutoff: Option<i32> = None;
                        let mut anonymise_conversions = true;
                        match rules.get("advertising_conversion_event") {
                            Some((action, days)) => {
                                if action != "anonymise" && action != "delete" {
                                    tracing::warn!(
                                        action = %action,
                                        "unexpected retention action for advertising_conversion_event; treating as anonymise"
                                    );
                                }
                                // `anonymise` (the registered default) strips
                                // personal data before the drop; `delete`
                                // drops rows as-is.
                                anonymise_conversions = action != "delete";
                                conversion_cutoff = Some(*days as i32);
                            }
                            None => tracing::warn!(
                                "privacy.retention_rules has no advertising_conversion_event row; conversion partition drops are skipped this pass"
                            ),
                        }

                        let mut metrics_cutoff: Option<i32> = None;
                        match rules.get("advertising_metric") {
                            Some((action, days)) if action == "delete" => {
                                metrics_cutoff = Some(*days as i32);
                            }
                            Some((action, _)) => tracing::warn!(
                                action = %action,
                                "privacy.retention_rules advertising_metric action is not delete; metrics partition drops are skipped this pass"
                            ),
                            None => tracing::warn!(
                                "privacy.retention_rules has no advertising_metric row; metrics partition drops are skipped this pass"
                            ),
                        }

                        // A NULL cutoff skips that class entirely; the SQL
                        // function never acts on a missing rule. Under
                        // `anonymise` the function strips personal data and
                        // retains the partition; under `delete` it drops.
                        sqlx::query(
                            "SELECT advertising.enforce_retention(
                               CASE WHEN $1::int IS NULL THEN NULL
                                    ELSE current_date - $1::int END,
                               CASE WHEN $2::int IS NULL THEN NULL
                                    ELSE current_date - $2::int END,
                               $3::boolean
                             )",
                        )
                        .bind(conversion_cutoff)
                        .bind(metrics_cutoff)
                        .bind(anonymise_conversions)
                        .execute(&pool)
                        .await?;

                        // TASK-028 (FR-1130): the run ledger. One row per
                        // advertising class this pass actually acted on,
                        // counted from the enforcement log of the last day
                        // — the retention report and the staleness alert
                        // read these rows, never a promise.
                        let acted: Vec<(String, i64)> = sqlx::query_as(
                            "SELECT parent_name, count(*)
                             FROM advertising.partition_retention_log
                             WHERE recorded_at > now() - interval '25 hours'
                             GROUP BY parent_name",
                        )
                        .fetch_all(&pool)
                        .await?;
                        let class_of = |parent: &str| {
                            if parent == "conversion_events" {
                                Some("advertising_conversion_event")
                            } else if parent == "metrics_daily" {
                                Some("advertising_metric")
                            } else {
                                None
                            }
                        };
                        let conversion_action = if anonymise_conversions {
                            "anonymise"
                        } else {
                            "delete"
                        };
                        for (parent, count) in acted {
                            if let Some(class) = class_of(&parent) {
                                let action = if class == "advertising_metric" {
                                    "delete"
                                } else {
                                    conversion_action
                                };
                                sqlx::query(
                                    "INSERT INTO privacy.retention_runs
                                       (data_class, action, rows_affected, executor, detail)
                                     VALUES ($1, $2, $3, 'platform.partition_maintenance',
                                             '{\"per_parent_actions\": true}'::jsonb)",
                                )
                                .bind(class)
                                .bind(action)
                                .bind(count)
                                .execute(&pool)
                                .await?;
                            }
                        }
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
            "advertising.metrics_ingestion",
            Duration::from_secs(config.scheduler.advertising_metrics_ingestion_interval_secs),
            {
                let sweep = advertising_metrics_ingestion_sweep.clone();
                move || {
                    let sweep = sweep.clone();
                    Box::pin(async move {
                        let report = sweep
                            .execute(200)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if report.connections_checked > 0 || report.errors > 0 {
                            tracing::info!(
                                connections_checked = report.connections_checked,
                                rows_ingested = report.rows_ingested,
                                errors = report.errors,
                                "advertising metrics ingestion sweep pass"
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "advertising.token_refresh",
            Duration::from_secs(config.scheduler.advertising_token_refresh_interval_secs),
            {
                let sweep = advertising_token_refresh_sweep.clone();
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
                                refreshed = report.refreshed,
                                failed = report.failed,
                                errors = report.errors,
                                "advertising token refresh sweep pass"
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
            Duration::from_secs(
                config
                    .scheduler
                    .advertising_conversion_retraction_interval_secs,
            ),
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
            "advertising.audience_sync",
            Duration::from_secs(config.scheduler.advertising_audience_sync_interval_secs),
            {
                let sweep = advertising_audience_sync_sweep.clone();
                move || {
                    let sweep = sweep.clone();
                    Box::pin(async move {
                        let report = sweep
                            .execute(3600, 200)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if report.checked > 0 {
                            tracing::info!(
                                checked = report.checked,
                                synced = report.synced,
                                failed = report.failed,
                                removed_identifiers = report.removed_identifiers,
                                unpropagated_removals = report.unpropagated_removals,
                                "advertising audience sync sweep pass"
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        Arc::new(FnJob::new(
            "platform.archival_sweep",
            Duration::from_secs(config.scheduler.archival_sweep_interval_secs),
            {
                let pool = pool.clone();
                let storage = archival_storage.clone();
                let enabled = config.features.archival;
                let ids = ids.clone();
                move || {
                    let pool = pool.clone();
                    let storage = storage.clone();
                    let ids = ids.clone();
                    Box::pin(async move {
                        if !enabled {
                            return Ok(());
                        }
                        let Some(storage) = storage else {
                            return Ok(());
                        };
                        let mover = sanvi_archival::sweep::mover_for(&pool, ids, storage);
                        let report = sanvi_archival::sweep::archive_aged_classes(&pool, &mover)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        for class in &report.classes {
                            tracing::info!(
                                data_class = %class.data_class,
                                rows_archived = class.rows_archived,
                                archives_created = class.archives_created,
                                "archival sweep pass complete"
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
                                "privacy opt-out propagation pass",
                            );
                        }
                        Ok(())
                    })
                }
            },
        )),
        // TASK-029 (FR-1134): ledger every completed erasure against the
        // destinations outside the provider registry. A `failed` row keeps
        // its destination visible instead of letting silence look like
        // success.
        Arc::new(FnJob::new(
            "privacy.erasure_propagation",
            Duration::from_secs(config.scheduler.dsr_advance_interval_secs),
            {
                let propagate = erasure_propagation.clone();
                move || {
                    let propagate = propagate.clone();
                    Box::pin(async move {
                        let report = propagate
                            .execute(100)
                            .await
                            .map_err(|err| anyhow::anyhow!(err))?;
                        if report.requests_propagated > 0 || report.failed > 0 {
                            tracing::info!(
                                requests = report.requests_propagated,
                                destinations = report.destinations_recorded,
                                failed = report.failed,
                                "privacy erasure propagation pass",
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
        tx_manager.clone(),
        format!("worker-{}", uuid::Uuid::now_v7()),
        jobs,
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let management_listener = match tokio::net::TcpListener::bind((
        config.worker_management.host.as_str(),
        config.worker_management.port,
    ))
    .await
    {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(%error, "failed to bind worker management listener");
            return ExitCode::FAILURE;
        }
    };
    let worker_health = management::WorkerHealth::starting();
    let management_task = {
        let metrics = metrics.clone();
        let health = worker_health.clone();
        let pool = pool.clone();
        let redis = config.redis.clone();
        let shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            management::run(management_listener, metrics, health, pool, redis, shutdown)
                .await
                .expect("worker management server failed");
        })
    };
    let signal_shutdown = shutdown_tx.clone();
    tokio::spawn(async move {
        sanvi_http::shutdown_signal().await;
        tracing::info!("shutdown signal received; draining worker");
        let _ = signal_shutdown.send(true);
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
    let ssr_for_localization = ssr_cache.clone();
    let localization_task = tokio::spawn(async move {
        localization_consumer
            .run_until_shutdown(
                &localization_stream,
                "localization",
                &localization_consumer_name,
                localization_event_handler(localization_service, Some(ssr_for_localization)),
                localization_shutdown,
            )
            .await;
    });

    let theming_shutdown = shutdown_rx.clone();
    let theming_stream = format!("{}.theming", config.outbox.stream);
    let ssr_for_theming = ssr_cache.clone();
    let theming_task = tokio::spawn(async move {
        theming_consumer
            .run_until_shutdown(
                &theming_stream,
                "theming",
                &theming_consumer_name,
                theming_event_handler(theming_cache, Some(ssr_for_theming)),
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

    let dispatcher_shutdown = shutdown_rx.clone();
    let dispatcher_task = tokio::spawn(async move {
        dispatcher
            .run_until_shutdown(
                Duration::from_millis(config.outbox.poll_interval_ms),
                dispatcher_shutdown,
            )
            .await;
    });

    let notif_tasks: Vec<tokio::task::JoinHandle<()>> = if !notifications_enabled {
        Vec::new()
    } else {
        let notif_gateway = Arc::new(
            HttpNotificationGateway::new(config.notification.clone())
                .with_abuse_counter(
                    Arc::new(sanvi_http::abuse::AbuseCounter::new(Some(redis.clone()))),
                    config.notification.send_quota_limit,
                    config.notification.send_quota_window_secs,
                )
                .with_commercial_quota(quota_runtime.clone()),
        );
        let notif_directory = Arc::new(WorkerRecipientDirectory {
            users: Arc::new(sanvi_identity::adapters::repo::SqlxUserRepository::new()),
            tx_manager: tx_manager.clone(),
            memberships: Arc::new(sanvi_identity::adapters::repo::SqlxMembershipRepository::new()),
            roles: Arc::new(SqlxRoleCatalog::new(
                tx_manager.clone(),
                Arc::new(SqlxRoleRepository::new()),
            )),
        });
        let notif_renderer = Arc::new(WorkerCopyRenderer {
            localizer: notification_localizer.clone(),
        });

        type IntentMapper = fn(
            &sanvi_kernel::EventEnvelope,
        ) -> Option<sanvi_kernel::notifications::NotificationIntent>;
        type ConsumerSpec = (&'static str, &'static str, IntentMapper);

        let consumers: [ConsumerSpec; 6] = [
            (
                "identity",
                "notifications-identity",
                sanvi_identity::adapters::notification_intent,
            ),
            (
                "billing",
                "notifications-billing",
                sanvi_billing::adapters::notification_intent,
            ),
            (
                "payments",
                "notifications-payments",
                sanvi_payments::adapters::notification_intent,
            ),
            (
                "privacy",
                "notifications-privacy",
                sanvi_privacy::adapters::notification_intent,
            ),
            (
                "domains",
                "notifications-domains",
                sanvi_domains::adapters::notification_intent,
            ),
            (
                "advertising",
                "notifications-advertising",
                sanvi_advertising::adapters::notification_intent,
            ),
        ];

        let mut tasks = Vec::with_capacity(consumers.len());
        for (stream_suffix, group_name, mapper) in consumers {
            let stream_name = format!("{}.{stream_suffix}", config.outbox.stream);
            let consumer_name = format!("{group_name}-{}", uuid::Uuid::now_v7());
            let shutdown = shutdown_rx.clone();

            let metrics = Arc::new(ConsumerMetrics::new(group_name));
            if let Err(err) = metrics.register(registry) {
                eprintln!("failed to register {group_name} consumer metrics: {err}");
                return ExitCode::from(1);
            }

            let consumer = EventConsumer::postgres(tx_manager.clone()).with_metrics(metrics);
            let dispatcher = Arc::new(NotificationDispatcher::new(
                notif_gateway.clone(),
                notif_directory.clone(),
                notif_renderer.clone(),
                mapper,
                config.notification.max_event_age_secs,
            ));
            let handler = dispatcher.into_consumer_handler();

            tasks.push(tokio::spawn(async move {
                consumer
                    .run_until_shutdown_at(
                        &stream_name,
                        group_name,
                        &consumer_name,
                        handler,
                        shutdown,
                        "0",
                    )
                    .await;
            }));
        }
        tasks
    };

    let mut tasks = vec![
        ("management", management_task),
        ("outbox", dispatcher_task),
        ("tenancy", tenancy_task),
        ("localization", localization_task),
        ("theming", theming_task),
        ("domains", domains_task),
        ("access", access_task),
        ("entitlements", entitlement_task),
        ("billing", billing_task),
        ("scheduler", scheduler_task),
    ];
    tasks.extend(notif_tasks.into_iter().map(|task| ("notifications", task)));
    let outcome = management::supervise(tasks, shutdown_tx, worker_health).await;

    telemetry_guard.shutdown();
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "worker supervision failed");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod config_tests {
    use super::{FeaturesConfig, WorkerConfig, WorkerPoolConfig, notifications_disabled_reason};
    use sanvi_kernel::IdsConfig;
    use sanvi_persistence::DatabaseConfig;

    fn configured() -> WorkerConfig {
        let mut config = WorkerConfig::default();
        config.notification.base_url = "http://localhost:58080".to_string();
        config
    }

    #[test]
    fn an_absent_features_section_leaves_notifications_on() {
        // `#[serde(default)]` on the field means an absent `[features]`
        // section resolves to `FeaturesConfig::default()`. That default must
        // match the serde key default, or deploying without the section would
        // silently stop every outbound notification — the opposite of what a
        // missing rollback switch should do.
        assert!(FeaturesConfig::default().notifications);
        assert!(WorkerConfig::default().features.notifications);
    }

    #[test]
    fn the_feature_flag_stops_worker_notifications_even_when_configured() {
        let mut config = configured();
        assert_eq!(notifications_disabled_reason(&config), None);

        config.features = FeaturesConfig {
            archival: false,
            notifications: false,
            advertising: false,
        };
        // Both the Lane A consumers and the Lane B export mail read this one
        // decision, so the flag is a real rollback switch for the worker and
        // not just for the api.
        assert_eq!(
            notifications_disabled_reason(&config),
            Some("features.notifications is off")
        );
    }

    #[test]
    fn an_unset_base_url_is_reported_separately_from_the_flag() {
        let config = WorkerConfig::default();
        assert_eq!(
            notifications_disabled_reason(&config),
            Some("notification service base_url is empty")
        );
    }

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

    #[test]
    fn an_absent_worker_pool_section_inherits_the_shared_database_values() {
        let mut database = DatabaseConfig::default();
        let before = database.max_connections;
        WorkerPoolConfig::default().apply(&mut database);

        assert_eq!(database.max_connections, before);
        assert_eq!(database.statement_timeout_ms, 10_000);
    }

    #[test]
    fn the_worker_pool_overlay_overrides_only_the_keys_it_carries() {
        let overlay: WorkerPoolConfig =
            serde_json::from_str(r#"{ "max_connections": 8, "statement_timeout_ms": 60000 }"#)
                .unwrap();
        let mut database = DatabaseConfig::default();
        overlay.apply(&mut database);

        assert_eq!(database.max_connections, 8);
        assert_eq!(database.statement_timeout_ms, 60_000);
        // Everything the overlay does not carry keeps the shared value.
        assert_eq!(database.min_connections, 1);
        assert_eq!(database.acquire_timeout_secs, 10);
    }

    #[test]
    fn a_worker_pool_below_the_shared_minimum_is_rejected_after_the_merge() {
        // An overlay that asks for fewer than `[database]`'s min_connections
        // must fail validation — the check runs on the merged pair, which is
        // what `build_pool` will see, not on the section in isolation.
        let overlay: WorkerPoolConfig =
            serde_json::from_str(r#"{ "max_connections": 2, "min_connections": 4 }"#).unwrap();
        let mut database = DatabaseConfig::default();
        overlay.apply(&mut database);

        assert!(database.validate().is_err());
    }

    #[test]
    fn the_worker_presents_a_distinct_application_name() {
        let overlay: WorkerPoolConfig =
            serde_json::from_str(r#"{ "application_name": "sanvi-worker" }"#).unwrap();
        let mut database = DatabaseConfig::default();
        overlay.apply(&mut database);

        assert_eq!(database.application_name, "sanvi-worker");
        // And an empty override is a config error, not a silent blank name.
        let blank: WorkerPoolConfig =
            serde_json::from_str(r#"{ "application_name": " " }"#).unwrap();
        blank.apply(&mut database);
        assert!(database.validate().is_err());
    }
}
