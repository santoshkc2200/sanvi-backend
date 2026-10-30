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
        SqlxEntitlementRepository, SqlxQuotaUsageStore, SqlxRoleCatalog, SqlxRoleRepository,
        sync::entitlement_sync_handler,
    },
    application::ExpireEntitlements,
    domain::{
        Clock, EntitlementCache, EntitlementRepository, EventPublisher, PermissionStore,
        QuotaUsageStore, RoleRepository,
    },
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
        CleanupExpiredExports, GenerateAnnualMetrics, ProcessDsrBatch, PropagateErasure,
        PropagateOptOuts, RetentionSweep,
    },
    domain::{
        AppealRepository, AuthorizedAgentRepository, BreachRepository, ConsentRepository,
        DestinationRetainedItem, DirectiveRepository, DsrRepository, EnvelopeCrypto,
        EraseDestinationOutcome, ErasureDestination, ExportArtifactRepository, Hasher,
        JurisdictionRepository, LegalHoldRepository, Notifications as PrivacyNotificationsTrait,
        ParticipantRepository, PrivacyError, ProviderRegistry, RequestMetricsRepository,
        RetentionRuleRepository, SignalRepository, SubProcessorNotifier, SubProcessorRepository,
        SubjectKeyRepository, SubjectRef, SubjectRepository, VerificationRepository,
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
    /// Pre-create current and following partitions for every registry
    /// parent (TASK-028: one job, one registry — audit, outbox
    /// and the webhook histories). This starts before any conversion
    /// endpoint exists: partition maintenance cannot be an afterthought on
    /// the first day of a new month.
    pub partition_maintenance_interval_secs: u64,
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
            partition_maintenance_interval_secs: 86_400,
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
    #[serde(default)]
    pub domains_service: sanvi_domains::adapters::remote::ServiceConfig,
    /// Independently deployed Payments service.
    #[serde(default)]
    pub payments_service: sanvi_payments::adapters::remote::ServiceConfig,
    #[serde(default)]
    pub advertising_service: sanvi_advertising::adapters::remote::ServiceConfig,
    #[serde(default)]
    pub notification: sanvi_notifications::NotificationConfig,
    #[serde(default)]
    pub product_access: sanvi_product_access::ProductAccessConfig,
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
        self.domains_service.validate()?;
        self.payments_service.validate()?;
        self.advertising_service.validate()?;
        self.notification.validate()?;
        self.product_access.validate()?;
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
    service: sanvi_advertising::adapters::remote::RemoteClient,
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
        use sanvi_advertising::adapters::remote::{Delegation, RequestBinding, TENANT_AUDIENCE};
        let body =
            serde_json::to_vec(_subject).map_err(|e| PrivacyError::Upstream(e.to_string()))?;
        let claims = Delegation::new(
            TENANT_AUDIENCE,
            "suppression",
            &RequestBinding::json("POST", "/internal/advertising/suppression", &body),
        );
        let response = self
            .service
            .send(&claims, body, "application/json", "")
            .await
            .map_err(|e| PrivacyError::Upstream(e.to_string()))?;
        if response.status != 200 {
            return Err(PrivacyError::ProcessingPending(
                "Advertising suppression is pending".into(),
            ));
        }
        #[derive(serde::Deserialize)]
        struct Report {
            retracted: u64,
            unpropagated: u64,
        }
        let report: Report = serde_json::from_slice(&response.body)
            .map_err(|e| PrivacyError::Upstream(e.to_string()))?;

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

    // --- domains context (phase 08): resolution-cache consumer.
    // Verification, issuance, renewal, health and order sweeps run in the
    // standalone domains-service.
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
        std::sync::Arc::new(
            sanvi_product_access::adapters::ProductAccessPrivacyProvider::from_config(
                &config.product_access,
            )
            .expect("product_access privacy provider construction"),
        ),
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
            sanvi_domains::adapters::privacy_provider::RemoteDomainsPrivacyProvider::new(
                &config.domains_service,
            )
            .expect("validated domains service configuration for privacy provider"),
        ),
        std::sync::Arc::new(PrivacyContextProvider::new(
            privacy_tx.clone(),
            privacy_subjects.clone(),
            privacy_consents.clone(),
            privacy_signals.clone(),
            privacy_directives.clone(),
        )),
        std::sync::Arc::new(
            sanvi_payments::adapters::RemotePaymentsPrivacyProvider::new(&config.payments_service)
                .expect("validated payments service configuration for privacy provider"),
        ),
        std::sync::Arc::new(
            sanvi_advertising::adapters::RemoteAdvertisingPrivacyProvider::new(
                &config.advertising_service,
            )
            .expect("validated Advertising service configuration"),
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
    let verify_audit = Arc::new(VerifyAuditChain::new(
        tx_manager.clone(),
        audit_reader,
        config.audit.verify_batch_size,
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
            service: sanvi_advertising::adapters::remote::RemoteClient::new(
                &config.advertising_service,
            )
            .expect("validated Advertising service configuration"),
        }),
    ];
    let erasure_propagation = Arc::new(PropagateErasure::new(
        privacy_tx.clone(),
        erasure_destinations,
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
