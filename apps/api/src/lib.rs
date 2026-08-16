//! `sanvi-api` — the composition root for the HTTP application.
//!
//! `lib.rs` exposes [`AppConfig`], [`build_app`] and the merged OpenAPI
//! document so tests (and the CLI's contract check) can reuse them;
//! `main.rs` is a thin binary: read config → init telemetry → connect
//! infrastructure → inject adapters into use cases → serve.

pub mod config;
pub mod openapi;

use std::future::Future;
use std::sync::Arc;

use actix_web::{
    App, HttpResponse,
    body::BoxBody,
    dev::{ServiceRequest, ServiceResponse},
    web,
};
use chrono::{DateTime, Utc};
use sanvi_access::{
    AccessConfig,
    adapters::{
        HttpKetoClient, InMemoryDecisionCache, InMemoryEntitlementCache, OutboxEventPublisher,
        RedisEntitlementCache, SqlxEntitlementRepository, SqlxFeatureRepository,
        SqlxMembershipSource, SqlxQuotaUsageStore, SqlxRoleRepository,
    },
    application::entitlements::EntitlementPolicy,
    application::{
        CachedAuthorizer, CheckPermission, CreateCustomRole, DefineFeature, DeleteCustomRole,
        DeprecateFeature, EntitlementFeatureGate, EntitlementResolver, ExpireEntitlements,
        GrantEntitlement, GrantEntitlementOutput, ListFeatures, ListPermissions, ListRoles,
        ListTenantEntitlements, ListTenantOverrides, RebuildAccessTuples, RevokeEntitlement,
        SeedFeatures, UpdateCustomRole, UpdateFeature,
    },
    domain::{
        ApprovalGate, Authorizer, DecisionCache, EntitlementCache, EntitlementRepository,
        EventPublisher as AccessEventPublisher, FeatureGate, FeatureRepository, PermissionStore,
        QuotaUsageStore, RoleRepository,
    },
};
use sanvi_audit::{
    AuditConfig,
    adapters::{SqlxAuditReader, SqlxAuditWriter},
    application::{ListAuditLog, VerifyAuditChain},
    domain::{AuditReader, AuditWriter},
};
use sanvi_billing::{
    BillingConfig,
    adapters::{
        AccessEntitlementSyncer, CachedTaxGate, HttpStripeClient, SqlxInvoiceRepository,
        SqlxPlanEntitlementRepository, SqlxPlanPriceRepository, SqlxPlanRepository,
        SqlxSubscriptionRepository, SqlxWebhookEventRepository, StripeMetrics,
        SystemClock as BillingSystemClock, SystemIdGenerator as BillingSystemIdGenerator,
    },
    application::{
        AddPlanPrice, ApplySubscriptionOverride, CreateCheckoutSession, CreatePlan,
        CreatePortalSession, GetSubscription, ListInvoices, ListPlans, ListPublicPlans,
        RevokeSubscriptionOverride, SetPlanEntitlements, UpdatePlan, WebhookReceiver,
    },
    domain::{
        Clock as BillingClock, EntitlementSyncer, IdGenerator as BillingIds, InvoiceRepository,
        PlanEntitlementRepository, PlanPriceRepository, PlanRepository, StripeClient,
        SubscriptionRepository, TaxGate, WebhookEventRepository,
    },
    routes as billing_routes,
};
use sanvi_http::{
    AuthnMiddleware, FeatureGate as HttpFeatureGatePort, PermissionGate, ProblemDetail,
    ResourceRef, Subject, middleware,
};
use sanvi_identity::{
    IdentityConfig,
    adapters::{
        HttpKratosClient, KratosSessionVerifier, LoggingNotifications,
        OutboxEventPublisher as IdentityEventPublisher, RedisSessionCache, SystemClock,
        SystemIdGenerator,
        http::IdentityState,
        repo::{
            SqlxInvitationRepository, SqlxLinkIntentRepository, SqlxLinkedAccountRepository,
            SqlxMembershipRepository, SqlxUserRepository,
        },
    },
    application::{
        AcceptInvitation, CompleteLinkChallenge, GetMe, GrantMembership, HandleLogoutWebhook,
        InviteMember, ListInvitations, ListMembers, ListSessions, RemoveMember, ResendInvitation,
        RevokeInvitation, RevokeSession, StartLinkChallenge, SyncIdentityFromKratos,
        UpdateMemberRoles,
    },
    domain::{
        EventPublisher, InvitationRepository, KratosClient, LinkIntentRepository,
        LinkedAccountRepository, MembershipRepository, Notifications, RoleCatalog, SessionCache,
        UserRepository,
    },
    routes as identity_routes,
};
use sanvi_platform_admin::{
    PlatformAdminConfig,
    adapters::{
        ApprovalGateAdapter, OutboxEventPublisher as PlatformAdminEvents,
        SqlxImpersonationGrantRepository, SqlxImpersonationVerifier, SqlxPendingApprovalRepository,
        SqlxTenantAdminViewRepository, SystemClock as PlatformAdminClock,
        SystemIdGenerator as PlatformAdminIds,
    },
    application::approvals::{DecideApproval, RecordApprovalRequest},
    application::{
        ApproveRequest, CreateImpersonation, GetTenantAdminView, ListImpersonations,
        ListPendingApprovals, RejectRequest, RevokeImpersonation, SearchTenantAdminViews,
    },
    domain::{
        Clock as PlatformAdminClockTrait, EntitlementAdmin,
        EventPublisher as PlatformAdminEventsTrait, IdGenerator as PlatformAdminIdsTrait,
        ImpersonationGrantRepository, PendingApprovalRepository, PrivacyLock,
        TenantAdminViewRepository,
    },
    routes as platform_admin_routes,
};
use sanvi_privacy::{
    PrivacyConfig,
    adapters::{
        DisabledEnvelopeCrypto, GpcSignalMiddleware,
        LoggingNotifications as PrivacyLoggingNotifications, LoggingSubProcessorNotifier,
        OutboxEventPublisher as PrivacyEvents, RuleBasedObligationCalculator, Sha256Hasher,
        SqlxAppealRepository, SqlxAuthorizedAgentRepository, SqlxBreachRepository,
        SqlxConsentRepository, SqlxDirectiveRepository, SqlxDsrRepository, SqlxEnvelopeCrypto,
        SqlxExportArtifactRepository, SqlxJurisdictionRepository, SqlxLegalHoldRepository,
        SqlxParticipantRepository, SqlxProcessingActivityRepository, SqlxRequestMetricsRepository,
        SqlxRetentionRuleRepository, SqlxRiskAssessmentRepository, SqlxSignalRepository,
        SqlxSubProcessorRepository, SqlxSubjectKeyRepository, SqlxSubjectRepository,
        SqlxVerificationRepository, SystemClock as PrivacySystemClock,
        SystemIdGenerator as PrivacySystemIdGenerator, ZipPackager,
    },
    application::{
        AppealRequest, ApplyLegalHold, CleanupExpiredExports, DecideAppeal, DownloadExport,
        ExtendDeadline, GenerateAnnualMetrics, GetDirectives, GetRequestStatus, ListActivities,
        ListAssessments, ListHolds, ListIncidents, ListJurisdictions, ListObligations,
        ListOpenAppeals, ListRequestsOperator, ListRetentionRules, ListSubProcessors,
        MarkBreachNotified, NoticeService, PatchRequest, PersistedDirectiveResolver,
        ProcessDsrBatch, PropagateOptOuts, PublicMetrics, RecordBreachIncident,
        RecordConsentChange, RecordLimitSensitive, RecordOptOut,
        RejectRequest as PrivacyRejectRequest, ReleaseLegalHold, RemoveSubProcessor,
        RetentionSweep, SubmitDsr, SubmitTenantRequest, UpsertActivity, UpsertAssessment,
        UpsertJurisdiction, UpsertRetentionRule, UpsertSubProcessor, VerifyRequest,
    },
    domain::{
        AppealRepository, AuditWriter as PrivacyAuditWriter, AuthorizedAgentRepository,
        BreachObligationCalculator, BreachRepository, Clock as PrivacyClock, ConsentRepository,
        DirectiveRepository, DirectiveResolver, DsrRepository, EnvelopeCrypto,
        EventPublisher as PrivacyEventPublisher, ExportArtifactRepository, ExportPackager, Hasher,
        IdGenerator as PrivacyIds, JurisdictionRepository,
        LegalHoldRepository as PrivacyLegalHoldRepository, Notifications as PrivacyNotifications,
        ParticipantRepository, ProcessingActivityRepository as PrivacyActivityRepository,
        ProviderRegistry, RequestMetricsRepository, RetentionRuleRepository,
        RiskAssessmentRepository as PrivacyAssessmentRepository, SignalRepository,
        SubProcessorNotifier, SubProcessorRepository, SubjectKeyRepository, SubjectRepository,
        VerificationRepository,
    },
};
use sanvi_system::{
    SystemState,
    adapters::http::{api_routes, health_routes},
    adapters::{PostgresProbe, RedisProbe},
    application::{CheckLiveness, CheckReadiness, GetBuildInfo},
    domain::BuildInfo,
};
use sanvi_telemetry::MetricsRegistry;
use sanvi_tenancy::{
    TenancyConfig,
    adapters::{
        OutboxEventPublisher as TenancyEventPublisher, SystemClock as TenancyClock,
        SystemIdGenerator as TenancyIdGenerator,
        http::TenancyState,
        repo::{SqlxSlugReservationPort, SqlxTenantRepository, SqlxTenantSettingsRepository},
        resolution::HostTenantResolver,
    },
    application::{
        ActivateTenant, ArchiveTenant, CheckSlugAvailability, GetTenant, GetTenantSettings,
        ListTenants, ProvisionTenant, ResolveTenant, ResumeTenant, SuspendTenant,
        UpdateTenantSettings,
    },
    domain::{
        EventPublisher as TenancyEventPublisherTrait, IdGenerator as TenancyIdGeneratorTrait,
        SlugReservationPort, TenantRepository, TenantResolutionCache, TenantSettingsRepository,
    },
    routes as tenancy_routes,
};
use uuid::Uuid;

pub use config::AppConfig;
pub use openapi::ApiDoc;

/// State shared by every handler in the app.
#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub build_info: BuildInfo,
    pub metrics: Arc<MetricsRegistry>,
}

/// Build the system context's application state (use cases + probes).
pub fn build_system_state(
    build_info: BuildInfo,
    pool: sanvi_persistence::PgPool,
    redis: Option<redis::aio::ConnectionManager>,
    readiness_budget_secs: u64,
) -> SystemState {
    let mut probes: Vec<Arc<dyn sanvi_system::domain::HealthProbe>> = Vec::new();
    probes.push(Arc::new(PostgresProbe::new(pool)));
    if let Some(manager) = redis {
        probes.push(Arc::new(RedisProbe::new(Arc::new(manager))));
    }

    SystemState {
        build_info: Arc::new(GetBuildInfo::new(build_info.clone())),
        readiness: Arc::new(CheckReadiness::new(
            probes,
            std::time::Duration::from_secs(readiness_budget_secs),
        )),
        liveness: Arc::new(CheckLiveness::new()),
    }
}

/// Build the tenancy context's application state: real adapters wired into
/// every use case, plus the resolver the middleware stack consumes.
pub fn build_tenancy_state(
    pool: sanvi_persistence::PgPool,
    redis: Option<redis::aio::ConnectionManager>,
    config: &TenancyConfig,
) -> TenancyState {
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone());
    let tenants: Arc<dyn TenantRepository> = Arc::new(SqlxTenantRepository::new());
    let settings: Arc<dyn TenantSettingsRepository> = Arc::new(SqlxTenantSettingsRepository::new());
    let slugs: Arc<dyn SlugReservationPort> = Arc::new(SqlxSlugReservationPort::new());
    let events: Arc<dyn TenancyEventPublisherTrait> = Arc::new(TenancyEventPublisher::new());
    let ids: Arc<dyn TenancyIdGeneratorTrait> = Arc::new(TenancyIdGenerator);
    let clock: Arc<dyn sanvi_tenancy::domain::Clock> = Arc::new(TenancyClock);
    let cache: Arc<dyn TenantResolutionCache> = match &redis {
        Some(manager) => Arc::new(sanvi_tenancy::adapters::RedisTenantResolutionCache::new(
            manager.clone(),
            std::time::Duration::from_secs(config.resolution_cache_ttl_secs),
        )),
        None => Arc::new(sanvi_tenancy::adapters::NoopTenantResolutionCache),
    };

    let resolve = Arc::new(ResolveTenant::new(
        tx_manager.clone(),
        tenants.clone(),
        cache,
    ));
    let host_resolver = Arc::new(HostTenantResolver::new(
        resolve.clone(),
        config.base_domain.clone(),
        config.usable_keys(),
    ));

    TenancyState {
        provision: Arc::new(ProvisionTenant::new(
            tx_manager.clone(),
            tenants.clone(),
            slugs.clone(),
            events.clone(),
            ids,
            clock.clone(),
        )),
        activate: Arc::new(ActivateTenant::new(
            tx_manager.clone(),
            tenants.clone(),
            events.clone(),
            clock.clone(),
        )),
        suspend: Arc::new(SuspendTenant::new(
            tx_manager.clone(),
            tenants.clone(),
            events.clone(),
            clock.clone(),
        )),
        resume: Arc::new(ResumeTenant::new(
            tx_manager.clone(),
            tenants.clone(),
            events.clone(),
            clock.clone(),
        )),
        archive: Arc::new(ArchiveTenant::new(
            tx_manager.clone(),
            tenants.clone(),
            events.clone(),
            clock.clone(),
        )),
        get_tenant: Arc::new(GetTenant::new(tx_manager.clone(), tenants.clone())),
        list_tenants: Arc::new(ListTenants::new(tx_manager.clone(), tenants.clone())),
        get_settings: Arc::new(GetTenantSettings::new(tx_manager.clone(), settings.clone())),
        update_settings: Arc::new(UpdateTenantSettings::new(
            tx_manager.clone(),
            settings,
            events.clone(),
        )),
        slug_availability: Arc::new(CheckSlugAvailability::new(tx_manager.clone(), slugs)),
        resolve,
        host_resolver,
        idempotency: sanvi_persistence::IdempotencyStore::new(tx_manager.pool().clone()),
    }
}

/// The identity context's cross-context ports and use cases, plus the
/// session verifier the authn middleware consumes.
#[derive(Clone)]
pub struct IdentityWiring {
    pub state: IdentityState,
    pub verifier: Arc<dyn sanvi_http::SessionVerifier>,
    pub webhook: Arc<sanvi_identity::adapters::http::WebhookState>,
    pub memberships: Arc<dyn MembershipRepository>,
}

/// Build the identity context's application state.
pub fn build_identity_state(
    pool: sanvi_persistence::PgPool,
    redis: Option<redis::aio::ConnectionManager>,
    config: &IdentityConfig,
    kratos_public_url: &str,
    kratos_admin_url: &str,
    role_catalog: Arc<dyn RoleCatalog>,
) -> IdentityWiring {
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone());
    let users: Arc<dyn UserRepository> = Arc::new(SqlxUserRepository::new());
    let memberships: Arc<dyn MembershipRepository> = Arc::new(SqlxMembershipRepository::new());
    let linked: Arc<dyn LinkedAccountRepository> = Arc::new(SqlxLinkedAccountRepository::new());
    let invitations: Arc<dyn InvitationRepository> = Arc::new(SqlxInvitationRepository::new());
    let link_intents: Arc<dyn LinkIntentRepository> = Arc::new(SqlxLinkIntentRepository::new());
    let events: Arc<dyn EventPublisher> = Arc::new(IdentityEventPublisher::new());
    let notifications: Arc<dyn Notifications> = Arc::new(LoggingNotifications::new());
    let ids: Arc<dyn sanvi_identity::domain::IdGenerator> = Arc::new(SystemIdGenerator);
    let clock: Arc<dyn sanvi_identity::domain::Clock> = Arc::new(SystemClock);
    let kratos: Arc<dyn KratosClient> = Arc::new(
        HttpKratosClient::new(
            kratos_public_url.to_string(),
            Some(kratos_admin_url.to_string()),
        )
        .expect("kratos client construction is infallible"),
    );
    let session_cache: Arc<dyn SessionCache> = match &redis {
        Some(manager) => Arc::new(RedisSessionCache::new(manager.clone())),
        None => Arc::new(sanvi_identity::adapters::NoopSessionCache),
    };

    let verifier: Arc<dyn sanvi_http::SessionVerifier> = Arc::new(KratosSessionVerifier::new(
        kratos.clone(),
        session_cache.clone(),
        users.clone(),
        memberships.clone(),
        tx_manager.clone(),
        config.session_cache_ttl_secs,
    ));

    let sync = Arc::new(SyncIdentityFromKratos::new(
        tx_manager.clone(),
        users.clone(),
        linked.clone(),
        events.clone(),
        notifications.clone(),
        ids.clone(),
        clock.clone(),
        kratos.clone(),
        config.trusted_email_verified_providers.clone(),
    ));
    let logout = Arc::new(HandleLogoutWebhook::new(
        tx_manager.clone(),
        session_cache.clone(),
        users.clone(),
        events.clone(),
    ));

    let state = IdentityState {
        get_me: Arc::new(GetMe::new(tx_manager.clone(), users.clone())),
        list_sessions: Arc::new(ListSessions::new(kratos.clone())),
        revoke_session: Arc::new(RevokeSession::new(
            tx_manager.clone(),
            kratos.clone(),
            users.clone(),
            events.clone(),
            notifications.clone(),
            session_cache.clone(),
        )),
        start_link_challenge: Arc::new(StartLinkChallenge::new(
            tx_manager.clone(),
            link_intents.clone(),
            clock.clone(),
            config.link_intent_ttl_secs,
        )),
        complete_link_challenge: Arc::new(CompleteLinkChallenge::new(
            tx_manager.clone(),
            link_intents.clone(),
            linked.clone(),
            events.clone(),
            notifications.clone(),
            clock.clone(),
        )),
        list_members: Arc::new(ListMembers::new(tx_manager.clone(), memberships.clone())),
        grant_member: Arc::new(GrantMembership::new(
            tx_manager.clone(),
            users.clone(),
            memberships.clone(),
            role_catalog.clone(),
            events.clone(),
            clock.clone(),
        )),
        update_member_roles: Arc::new(UpdateMemberRoles::new(
            tx_manager.clone(),
            memberships.clone(),
            role_catalog.clone(),
            events.clone(),
        )),
        remove_member: Arc::new(RemoveMember::new(
            tx_manager.clone(),
            memberships.clone(),
            role_catalog.clone(),
            events.clone(),
        )),
        list_invitations: Arc::new(ListInvitations::new(
            tx_manager.clone(),
            invitations.clone(),
        )),
        invite_member: Arc::new(InviteMember::new(
            tx_manager.clone(),
            invitations.clone(),
            role_catalog.clone(),
            events.clone(),
            notifications.clone(),
            ids,
            clock.clone(),
            config.invitation_ttl_days,
        )),
        accept_invitation: Arc::new(AcceptInvitation::new(
            tx_manager.clone(),
            invitations.clone(),
            memberships.clone(),
            role_catalog.clone(),
            events.clone(),
            clock.clone(),
        )),
        resend_invitation: Arc::new(ResendInvitation::new(
            tx_manager.clone(),
            invitations.clone(),
            events.clone(),
            notifications.clone(),
            clock.clone(),
            config.invitation_ttl_days,
        )),
        revoke_invitation: Arc::new(RevokeInvitation::new(tx_manager.clone(), invitations)),
        link_intent_ttl_secs: config.link_intent_ttl_secs,
    };

    let webhook = Arc::new(sanvi_identity::adapters::http::WebhookState {
        sync,
        logout,
        secret: config.webhook_secret.clone(),
        allowed_clients: config.webhook_allowed_clients.clone(),
    });

    IdentityWiring {
        state,
        verifier,
        webhook,
        memberships,
    }
}

/// The access context's gates + state, implementing the platform's
/// `PermissionGate` and `FeatureGate` ports.
#[derive(Clone)]
pub struct AccessWiring {
    pub gate: Arc<dyn PermissionGate>,
    pub feature_gate: Arc<dyn HttpFeatureGatePort>,
    pub state: sanvi_access::adapters::http::AccessState,
    pub authorizer: Arc<dyn Authorizer>,
    pub role_repository: Arc<dyn RoleRepository>,
    pub permission_store: Arc<dyn PermissionStore>,
    pub decision_cache: Arc<dyn DecisionCache>,
    pub entitlement_cache: Arc<dyn EntitlementCache>,
    pub tx_manager: sanvi_persistence::TxManager,
    pub rebuild: Arc<RebuildAccessTuples>,
    /// The domain-level gate (worker/jobs consume it directly).
    pub domain_feature_gate: Arc<dyn FeatureGate>,
    pub grant_entitlement: Arc<GrantEntitlement>,
    pub revoke_entitlement: Arc<RevokeEntitlement>,
    pub list_tenant_overrides: Arc<ListTenantOverrides>,
    pub expire_entitlements: Arc<ExpireEntitlements>,
    pub seed_features: Arc<SeedFeatures>,
    pub features_repo: Arc<dyn FeatureRepository>,
    pub entitlements_repo: Arc<dyn EntitlementRepository>,
    pub quota_store: Arc<dyn QuotaUsageStore>,
    pub access_events: Arc<dyn AccessEventPublisher>,
}

/// The `PermissionGate` port implementation: converts between the platform's
/// subject/resource vocabulary and the access context's.
pub struct AccessGate {
    authorizer: Arc<dyn Authorizer>,
}

impl PermissionGate for AccessGate {
    fn check<'a>(
        &'a self,
        subject: &'a Subject,
        permission: &'a str,
        resource: &'a ResourceRef,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<sanvi_http::Decision, sanvi_http::ApiError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let permission = match sanvi_access::domain::Permission::parse(permission) {
                Ok(permission) => permission,
                Err(err) => {
                    tracing::warn!(error = %err, "gate received an unparsable permission");
                    return Ok(sanvi_http::Decision::Denied);
                }
            };
            let resource = match resource {
                ResourceRef::Tenant(id) => sanvi_access::domain::ResourceRef::Tenant(*id),
                ResourceRef::User(id) => sanvi_access::domain::ResourceRef::User(*id),
                ResourceRef::Platform => sanvi_access::domain::ResourceRef::Platform,
            };
            match self
                .authorizer
                .check(subject.user_id, &permission, &resource)
                .await
            {
                Ok(sanvi_access::domain::Decision::Allowed) => Ok(sanvi_http::Decision::Allowed),
                Ok(sanvi_access::domain::Decision::Denied) => Ok(sanvi_http::Decision::Denied),
                Err(err) => {
                    tracing::error!(error = %err, "authorization backend failure");
                    Err(sanvi_http::ApiError::ServiceUnavailable {
                        detail: "authorization is temporarily unavailable".to_string(),
                    })
                }
            }
        })
    }
}

/// The HTTP-level `FeatureGate` port implementation over the access
/// context's domain gate, with the phase-03 metrics
/// (`entitlement_check_duration`, `feature_gate_denied_total`).
pub struct AccessFeatureGate {
    gate: Arc<dyn FeatureGate>,
    denied: prometheus::IntCounterVec,
    duration: prometheus::Histogram,
}

impl AccessFeatureGate {
    pub fn new(gate: Arc<dyn FeatureGate>, registry: &prometheus::Registry) -> Self {
        let denied = prometheus::IntCounterVec::new(
            prometheus::Opts::new(
                "feature_gate_denied_total",
                "Feature gate checks that denied, per feature",
            ),
            &["feature"],
        )
        .expect("feature gate metric construction is infallible");
        let duration = prometheus::Histogram::with_opts(prometheus::HistogramOpts::new(
            "entitlement_check_duration",
            "Duration of entitlement gate checks",
        ))
        .expect("entitlement check histogram construction is infallible");
        let _ = registry.register(Box::new(denied.clone()));
        let _ = registry.register(Box::new(duration.clone()));
        Self {
            gate,
            denied,
            duration,
        }
    }
}

impl HttpFeatureGatePort for AccessFeatureGate {
    fn check<'a>(
        &'a self,
        tenant_id: Uuid,
        feature: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<bool, sanvi_http::ApiError>> + Send + 'a>>
    {
        Box::pin(async move {
            let started = std::time::Instant::now();
            let key = match sanvi_access::domain::FeatureKey::parse(feature) {
                Ok(key) => key,
                Err(err) => {
                    tracing::warn!(error = %err, "gate received an unparsable feature key");
                    self.denied.with_label_values(&[feature]).inc();
                    return Ok(false);
                }
            };
            let enabled = self.gate.is_enabled(tenant_id, &key).await.map_err(|err| {
                tracing::error!(error = %err, "entitlement backend failure");
                sanvi_http::ApiError::ServiceUnavailable {
                    detail: "entitlements are temporarily unavailable".to_string(),
                }
            })?;
            self.duration.observe(started.elapsed().as_secs_f64());
            if !enabled {
                self.denied.with_label_values(&[feature]).inc();
            }
            Ok(enabled)
        })
    }
}

/// Build the access context's application state.
#[allow(clippy::too_many_arguments)]
pub fn build_access_state(
    pool: sanvi_persistence::PgPool,
    config: &AccessConfig,
    keto_read_url: &str,
    keto_write_url: &str,
    memberships: Arc<dyn MembershipRepository>,
    audit_writer: Arc<dyn AuditWriter>,
    approval_gate: Arc<dyn ApprovalGate>,
    registry: &prometheus::Registry,
    redis: Option<redis::aio::ConnectionManager>,
) -> AccessWiring {
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone());
    let roles: Arc<dyn RoleRepository> = Arc::new(SqlxRoleRepository::new());
    let store: Arc<dyn PermissionStore> = Arc::new(
        HttpKetoClient::new(keto_read_url.to_string(), keto_write_url.to_string())
            .expect("keto client construction is infallible"),
    );
    let cache: Arc<dyn DecisionCache> = Arc::new(InMemoryDecisionCache::new(
        config.decision_cache_max_entries,
    ));
    let features: Arc<dyn FeatureRepository> = Arc::new(SqlxFeatureRepository::new());
    let entitlements: Arc<dyn EntitlementRepository> = Arc::new(SqlxEntitlementRepository::new());
    let quota: Arc<dyn QuotaUsageStore> = Arc::new(SqlxQuotaUsageStore::new());
    let events: Arc<dyn AccessEventPublisher> = Arc::new(OutboxEventPublisher::new());
    let clock: Arc<dyn sanvi_access::domain::Clock> = Arc::new(sanvi_access::adapters::SystemClock);
    let ids: Arc<dyn sanvi_access::domain::IdGenerator> =
        Arc::new(sanvi_access::adapters::SystemRoleIdGenerator);

    let check = Arc::new(CheckPermission::new(
        tx_manager.clone(),
        roles.clone(),
        store.clone(),
    ));
    let authorizer: Arc<dyn Authorizer> = Arc::new(CachedAuthorizer::new(
        check,
        cache.clone(),
        config.decision_cache_ttl_secs,
    ));
    let gate: Arc<dyn PermissionGate> = Arc::new(AccessGate {
        authorizer: authorizer.clone(),
    });

    let membership_source = Arc::new(SqlxMembershipSource::new(
        tx_manager.clone(),
        memberships.clone(),
    ));
    let rebuild = Arc::new(RebuildAccessTuples::new(
        tx_manager.clone(),
        roles.clone(),
        store.clone(),
        membership_source,
    ));

    let entitlement_cache: Arc<dyn EntitlementCache> = match &redis {
        Some(manager) => Arc::new(RedisEntitlementCache::new(
            manager.clone(),
            config.entitlement_cache_ttl_secs,
        )),
        None => Arc::new(InMemoryEntitlementCache::new(
            std::time::Duration::from_secs(config.entitlement_cache_ttl_secs),
            10_000,
        )),
    };

    let resolver = Arc::new(EntitlementResolver::new(
        tx_manager.clone(),
        features.clone(),
        entitlements.clone(),
        entitlement_cache.clone(),
        clock.clone(),
        config.entitlement_cache_ttl_secs,
    ));
    let domain_gate: Arc<dyn FeatureGate> = Arc::new(EntitlementFeatureGate::new(
        resolver.clone(),
        tx_manager.clone(),
        quota.clone(),
        events.clone(),
        clock.clone(),
    ));
    let http_gate: Arc<dyn HttpFeatureGatePort> =
        Arc::new(AccessFeatureGate::new(domain_gate.clone(), registry));

    let grant_entitlement = Arc::new(GrantEntitlement::new(
        tx_manager.clone(),
        features.clone(),
        entitlements.clone(),
        events.clone(),
        clock.clone(),
        audit_writer.clone(),
        approval_gate,
        EntitlementPolicy {
            approval_threshold: config.entitlement_approval_threshold,
        },
    ));
    let revoke_entitlement = Arc::new(RevokeEntitlement::new(
        tx_manager.clone(),
        entitlements.clone(),
        entitlement_cache.clone(),
        events.clone(),
        clock.clone(),
        audit_writer.clone(),
    ));
    let list_tenant_overrides = Arc::new(ListTenantOverrides::new(
        tx_manager.clone(),
        entitlements.clone(),
    ));
    let list_tenant_entitlements = Arc::new(ListTenantEntitlements::new(resolver.clone()));
    let expire_entitlements = Arc::new(ExpireEntitlements::new(
        tx_manager.clone(),
        entitlements.clone(),
        entitlement_cache.clone(),
        events.clone(),
        clock.clone(),
    ));
    let seed_features = Arc::new(SeedFeatures::new(tx_manager.clone(), features.clone()));

    let state = sanvi_access::adapters::http::AccessState {
        list_permissions: Arc::new(ListPermissions::new()),
        list_roles: Arc::new(ListRoles::new(tx_manager.clone(), roles.clone())),
        create_role: Arc::new(CreateCustomRole::new(
            tx_manager.clone(),
            roles.clone(),
            authorizer.clone(),
            events.clone(),
            ids,
            clock.clone(),
            audit_writer.clone(),
        )),
        update_role: Arc::new(UpdateCustomRole::new(
            tx_manager.clone(),
            roles.clone(),
            authorizer.clone(),
            events.clone(),
            clock.clone(),
            audit_writer.clone(),
        )),
        delete_role: Arc::new(DeleteCustomRole::new(
            tx_manager.clone(),
            roles.clone(),
            events.clone(),
            clock.clone(),
            audit_writer.clone(),
        )),
        list_features: Arc::new(ListFeatures::new(tx_manager.clone(), features.clone())),
        define_feature: Arc::new(DefineFeature::new(
            tx_manager.clone(),
            features.clone(),
            events.clone(),
            clock.clone(),
            audit_writer.clone(),
        )),
        update_feature: Arc::new(UpdateFeature::new(
            tx_manager.clone(),
            features.clone(),
            events.clone(),
            clock.clone(),
            audit_writer.clone(),
        )),
        deprecate_feature: Arc::new(DeprecateFeature::new(
            tx_manager.clone(),
            features.clone(),
            events.clone(),
            clock.clone(),
            audit_writer.clone(),
        )),
        list_tenant_entitlements,
    };

    AccessWiring {
        gate,
        feature_gate: http_gate,
        state,
        authorizer,
        role_repository: roles,
        permission_store: store,
        decision_cache: cache,
        entitlement_cache,
        tx_manager,
        rebuild,
        domain_feature_gate: domain_gate,
        grant_entitlement,
        revoke_entitlement,
        list_tenant_overrides,
        expire_entitlements,
        seed_features,
        features_repo: features,
        entitlements_repo: entitlements,
        quota_store: quota,
        access_events: events,
    }
}

/// The audit context's state: writer/reader ports and use cases.
#[derive(Clone)]
pub struct AuditWiring {
    pub writer: Arc<dyn AuditWriter>,
    pub reader: Arc<dyn AuditReader>,
    pub state: sanvi_audit::adapters::http::AuditState,
    pub verify: Arc<VerifyAuditChain>,
}

/// Build the audit context's application state.
pub fn build_audit_state(pool: sanvi_persistence::PgPool, config: &AuditConfig) -> AuditWiring {
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone());
    let writer: Arc<dyn AuditWriter> = Arc::new(SqlxAuditWriter::new());
    let reader: Arc<dyn AuditReader> = Arc::new(SqlxAuditReader::new());
    let list = Arc::new(ListAuditLog::new(tx_manager.clone(), reader.clone()));
    let verify = Arc::new(VerifyAuditChain::new(
        tx_manager,
        reader.clone(),
        config.verify_batch_size,
    ));
    AuditWiring {
        writer,
        reader,
        state: sanvi_audit::adapters::http::AuditState { list },
        verify,
    }
}

/// The `EntitlementAdmin` port implementation over the access context's
/// use cases (composition root only).
pub struct AccessEntitlementAdmin {
    grant: Arc<GrantEntitlement>,
    revoke: Arc<RevokeEntitlement>,
    list: Arc<ListTenantOverrides>,
}

impl AccessEntitlementAdmin {
    pub fn new(
        grant: Arc<GrantEntitlement>,
        revoke: Arc<RevokeEntitlement>,
        list: Arc<ListTenantOverrides>,
    ) -> Self {
        Self {
            grant,
            revoke,
            list,
        }
    }
}

fn map_access_err(
    err: sanvi_access::domain::AccessError,
) -> sanvi_platform_admin::domain::PlatformAdminError {
    use sanvi_access::domain::AccessError as A;
    use sanvi_platform_admin::domain::PlatformAdminError as P;
    match err {
        A::FeatureNotFound { key } => P::FeatureNotFound(key),
        A::InvalidFeatureKey(detail) | A::InvalidLimit(detail) | A::InvalidEntitlement(detail) => {
            P::InvalidEntitlement(detail)
        }
        A::QuotaExceeded {
            feature,
            limit,
            attempted,
        } => P::EntitlementConflict(format!(
            "quota exceeded for {feature}: limit {limit}, attempted {attempted}"
        )),
        other => P::InvalidEntitlement(other.to_string()),
    }
}

#[async_trait::async_trait]
impl EntitlementAdmin for AccessEntitlementAdmin {
    async fn grant_override(
        &self,
        actor_id: Uuid,
        tenant_id: Uuid,
        feature_key: String,
        enabled: bool,
        limit: Option<i64>,
        expires_at: Option<DateTime<Utc>>,
        reason: Option<String>,
    ) -> Result<
        sanvi_platform_admin::domain::GrantOverrideOutcome,
        sanvi_platform_admin::domain::PlatformAdminError,
    > {
        let feature =
            sanvi_access::domain::FeatureKey::parse(&feature_key).map_err(map_access_err)?;
        match self
            .grant
            .execute(
                actor_id,
                None,
                sanvi_access::application::GrantEntitlementCommand {
                    tenant_id,
                    feature_key: feature,
                    enabled,
                    limit,
                    expires_at,
                    reason,
                    skip_approval: false,
                },
            )
            .await
        {
            Ok(GrantEntitlementOutput::Granted) => {
                Ok(sanvi_platform_admin::domain::GrantOverrideOutcome::Granted)
            }
            Ok(GrantEntitlementOutput::ApprovalRequired { approval_id }) => Ok(
                sanvi_platform_admin::domain::GrantOverrideOutcome::ApprovalRequired {
                    approval_id,
                },
            ),
            Err(err) => Err(map_access_err(err)),
        }
    }

    async fn grant_override_approved(
        &self,
        requested_by: Uuid,
        tenant_id: Uuid,
        feature_key: String,
        enabled: bool,
        limit: Option<i64>,
        expires_at: Option<DateTime<Utc>>,
        reason: Option<String>,
    ) -> Result<(), sanvi_platform_admin::domain::PlatformAdminError> {
        let feature =
            sanvi_access::domain::FeatureKey::parse(&feature_key).map_err(map_access_err)?;
        self.grant
            .execute(
                requested_by,
                None,
                sanvi_access::application::GrantEntitlementCommand {
                    tenant_id,
                    feature_key: feature,
                    enabled,
                    limit,
                    expires_at,
                    reason,
                    skip_approval: true,
                },
            )
            .await
            .map(|_| ())
            .map_err(map_access_err)
    }

    async fn revoke_override(
        &self,
        actor_id: Uuid,
        tenant_id: Uuid,
        feature_key: String,
    ) -> Result<(), sanvi_platform_admin::domain::PlatformAdminError> {
        let feature =
            sanvi_access::domain::FeatureKey::parse(&feature_key).map_err(map_access_err)?;
        self.revoke
            .execute(
                actor_id,
                None,
                sanvi_access::application::RevokeEntitlementCommand {
                    tenant_id,
                    feature_key: feature,
                },
            )
            .await
            .map_err(map_access_err)
    }

    async fn list_overrides(
        &self,
        tenant_id: Uuid,
    ) -> Result<
        Vec<sanvi_access::domain::Entitlement>,
        sanvi_platform_admin::domain::PlatformAdminError,
    > {
        self.list.execute(tenant_id).await.map_err(map_access_err)
    }
}

/// The platform_admin context's state + the impersonation middleware parts.
#[derive(Clone)]
pub struct PlatformAdminWiring {
    pub state: sanvi_platform_admin::adapters::http::PlatformAdminState,
    pub impersonation: Arc<sanvi_http::ImpersonationMiddleware>,
    /// The four-eyes gate the access context consumes (records pending
    /// approvals when its policy requires them).
    pub approval_gate: Arc<dyn ApprovalGate>,
}

/// Build the platform_admin context's application state. `entitlement_admin`
/// is the composition root's adapter over the access context's use cases;
/// the returned `approval_gate` is the same adapter the access context's
/// `GrantEntitlement` consumes.
pub fn build_platform_admin_state(
    pool: sanvi_persistence::PgPool,
    config: &PlatformAdminConfig,
    tenants: Arc<dyn TenantRepository>,
    memberships: Arc<dyn MembershipRepository>,
    entitlements: Arc<dyn EntitlementRepository>,
    entitlement_admin: Arc<dyn EntitlementAdmin>,
    audit_writer: Arc<dyn AuditWriter>,
    privacy_lock: Arc<dyn PrivacyLock>,
) -> PlatformAdminWiring {
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone());
    let grants: Arc<dyn ImpersonationGrantRepository> =
        Arc::new(SqlxImpersonationGrantRepository::new());
    let approvals: Arc<dyn PendingApprovalRepository> =
        Arc::new(SqlxPendingApprovalRepository::new());
    let clock: Arc<dyn PlatformAdminClockTrait> = Arc::new(PlatformAdminClock);
    let ids: Arc<dyn PlatformAdminIdsTrait> = Arc::new(PlatformAdminIds);
    let events: Arc<dyn PlatformAdminEventsTrait> = Arc::new(PlatformAdminEvents::new());

    let views: Arc<dyn TenantAdminViewRepository> = Arc::new(SqlxTenantAdminViewRepository::new(
        tenants,
        memberships.clone(),
        entitlements,
        grants.clone(),
        clock.clone(),
    ));

    let create_impersonation = Arc::new(CreateImpersonation::new(
        tx_manager.clone(),
        grants.clone(),
        memberships.clone(),
        privacy_lock,
        events.clone(),
        ids.clone(),
        clock.clone(),
        audit_writer.clone(),
        config.impersonation_max_minutes,
    ));
    let list_impersonations = Arc::new(ListImpersonations::new(
        tx_manager.clone(),
        grants.clone(),
        clock.clone(),
    ));
    let revoke_impersonation = Arc::new(RevokeImpersonation::new(
        tx_manager.clone(),
        grants.clone(),
        events.clone(),
        clock.clone(),
        audit_writer.clone(),
    ));

    let record_approval = Arc::new(RecordApprovalRequest::new(
        approvals.clone(),
        ids.clone(),
        clock.clone(),
    ));
    let decide = Arc::new(DecideApproval::new(
        tx_manager.clone(),
        approvals.clone(),
        entitlement_admin.clone(),
        clock.clone(),
        audit_writer.clone(),
    ));

    let verifier = Arc::new(SqlxImpersonationVerifier::new(
        grants.clone(),
        memberships,
        tx_manager.clone(),
        clock,
    ));
    let impersonation = Arc::new(sanvi_http::ImpersonationMiddleware { verifier });

    let state = sanvi_platform_admin::adapters::http::PlatformAdminState {
        search_tenants: Arc::new(SearchTenantAdminViews::new(
            tx_manager.clone(),
            views.clone(),
        )),
        get_tenant: Arc::new(GetTenantAdminView::new(tx_manager.clone(), views)),
        create_impersonation,
        list_impersonations,
        revoke_impersonation,
        entitlements: entitlement_admin,
        list_approvals: Arc::new(ListPendingApprovals::new(
            tx_manager.clone(),
            approvals.clone(),
        )),
        approve: Arc::new(ApproveRequest::new(decide.clone())),
        reject: Arc::new(RejectRequest::new(decide)),
    };

    let approval_gate: Arc<dyn ApprovalGate> = Arc::new(ApprovalGateAdapter::new(record_approval));

    PlatformAdminWiring {
        state,
        impersonation,
        approval_gate,
    }
}

/// The privacy context's cross-context wiring: HTTP state, the GPC
/// middleware, the directive resolver every other context consults, the
/// provider registry the DSR saga fans out over, the worker jobs and the
/// jurisdiction-driven impersonation lock.
#[derive(Clone)]
pub struct PrivacyWiring {
    pub state: sanvi_privacy::adapters::http::PrivacyState,
    pub gpc: GpcSignalMiddleware,
    pub resolver: Arc<dyn DirectiveResolver>,
    pub registry: ProviderRegistry,
    pub process_dsr: Arc<ProcessDsrBatch>,
    pub propagate_opt_outs: Arc<PropagateOptOuts>,
    pub retention_sweep: Arc<RetentionSweep>,
    pub cleanup_exports: Arc<CleanupExpiredExports>,
    pub generate_metrics: Arc<GenerateAnnualMetrics>,
    pub lock: Arc<dyn sanvi_platform_admin::domain::PrivacyLock>,
}

/// The jurisdiction-driven privacy lock (phase 05 wires the real
/// implementation): a tenant with an open erasure request cannot be
/// impersonated.
pub struct DsrPrivacyLock {
    tx_manager: sanvi_persistence::TxManager,
    requests: Arc<dyn DsrRepository>,
}

impl DsrPrivacyLock {
    pub fn new(tx_manager: sanvi_persistence::TxManager, requests: Arc<dyn DsrRepository>) -> Self {
        Self {
            tx_manager,
            requests,
        }
    }
}

#[async_trait::async_trait]
impl sanvi_platform_admin::domain::PrivacyLock for DsrPrivacyLock {
    async fn is_locked(
        &self,
        tenant_id: uuid::Uuid,
    ) -> Result<bool, sanvi_platform_admin::domain::PlatformAdminError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_lock_check", "privacy")
            .await
            .map_err(|err| {
                sanvi_platform_admin::domain::PlatformAdminError::Storage(err.to_string())
            })?;
        let locked = self
            .requests
            .has_open_erasure_for_tenant(&mut scope, tenant_id)
            .await
            .map_err(|err| {
                sanvi_platform_admin::domain::PlatformAdminError::Storage(err.to_string())
            })?;
        scope.commit().await.map_err(|err| {
            sanvi_platform_admin::domain::PlatformAdminError::Storage(err.to_string())
        })?;
        Ok(locked)
    }
}

/// Build the privacy context's application state. `providers` is the
/// registry built from every context crate's provider (missing one fails
/// the CI registry test); the audit adapter records every privacy action in
/// the phase-03 audit chain.
#[allow(clippy::too_many_arguments)]
pub fn build_privacy_state(
    pool: sanvi_persistence::PgPool,
    config: &PrivacyConfig,
    providers: ProviderRegistry,
    audit_writer: Arc<dyn sanvi_audit::domain::AuditWriter>,
) -> PrivacyWiring {
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone());
    let audit: Arc<dyn PrivacyAuditWriter> = Arc::new(
        sanvi_audit::adapters::PrivacyAuditWriterAdapter::new(audit_writer.clone()),
    );

    let subjects: Arc<dyn SubjectRepository> = Arc::new(SqlxSubjectRepository::new());
    let jurisdictions: Arc<dyn JurisdictionRepository> =
        Arc::new(SqlxJurisdictionRepository::new());
    let requests: Arc<dyn DsrRepository> = Arc::new(SqlxDsrRepository::new());
    let appeals: Arc<dyn AppealRepository> = Arc::new(SqlxAppealRepository::new());
    let participants: Arc<dyn ParticipantRepository> = Arc::new(SqlxParticipantRepository::new());
    let consents: Arc<dyn ConsentRepository> = Arc::new(SqlxConsentRepository::new());
    let signals: Arc<dyn SignalRepository> = Arc::new(SqlxSignalRepository::new());
    let directives: Arc<dyn DirectiveRepository> = Arc::new(SqlxDirectiveRepository::new());
    let holds: Arc<dyn PrivacyLegalHoldRepository> = Arc::new(SqlxLegalHoldRepository::new());
    let keys: Arc<dyn SubjectKeyRepository> = Arc::new(SqlxSubjectKeyRepository::new());
    let artifacts: Arc<dyn ExportArtifactRepository> =
        Arc::new(SqlxExportArtifactRepository::new());
    let challenges: Arc<dyn VerificationRepository> = Arc::new(SqlxVerificationRepository::new());
    let agents: Arc<dyn AuthorizedAgentRepository> = Arc::new(SqlxAuthorizedAgentRepository::new());
    let processors: Arc<dyn SubProcessorRepository> = Arc::new(SqlxSubProcessorRepository::new());
    let breaches: Arc<dyn BreachRepository> = Arc::new(SqlxBreachRepository::new());
    let rules: Arc<dyn RetentionRuleRepository> = Arc::new(SqlxRetentionRuleRepository::new());
    let metrics: Arc<dyn RequestMetricsRepository> = Arc::new(SqlxRequestMetricsRepository::new());
    let activities: Arc<dyn PrivacyActivityRepository> =
        Arc::new(SqlxProcessingActivityRepository::new());
    let assessments: Arc<dyn PrivacyAssessmentRepository> =
        Arc::new(SqlxRiskAssessmentRepository::new());

    let events: Arc<dyn PrivacyEventPublisher> = Arc::new(PrivacyEvents::new());
    let notifications: Arc<dyn PrivacyNotifications> = Arc::new(PrivacyLoggingNotifications);
    let clock: Arc<dyn PrivacyClock> = Arc::new(PrivacySystemClock);
    let ids: Arc<dyn PrivacyIds> = Arc::new(PrivacySystemIdGenerator);
    let hasher: Arc<dyn Hasher> = Arc::new(Sha256Hasher::new(if config.kek.is_empty() {
        "dev-pepper-insecure".to_string()
    } else {
        config.kek.clone()
    }));
    let crypto: Arc<dyn EnvelopeCrypto> = if config.encryption_enabled() {
        let kek: [u8; 32] =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &config.kek)
                .expect("validated by PrivacyConfig")
                .try_into()
                .expect("validated length");
        Arc::new(SqlxEnvelopeCrypto::new(
            tx_manager.clone(),
            keys.clone(),
            kek,
            config.kek_id.clone(),
        ))
    } else {
        Arc::new(DisabledEnvelopeCrypto)
    };
    let packager: Arc<dyn ExportPackager> = Arc::new(ZipPackager::new());
    let calculator: Arc<dyn BreachObligationCalculator> =
        Arc::new(RuleBasedObligationCalculator::new());
    let notifier: Arc<dyn SubProcessorNotifier> = Arc::new(LoggingSubProcessorNotifier);

    let resolver: Arc<dyn DirectiveResolver> = Arc::new(PersistedDirectiveResolver::new(
        tx_manager.clone(),
        subjects.clone(),
        jurisdictions.clone(),
        consents.clone(),
        signals.clone(),
        directives.clone(),
        clock.clone(),
    ));

    let submit_dsr = Arc::new(SubmitDsr::new(
        tx_manager.clone(),
        subjects.clone(),
        jurisdictions.clone(),
        requests.clone(),
        participants.clone(),
        challenges.clone(),
        agents.clone(),
        consents.clone(),
        directives.clone(),
        signals.clone(),
        events.clone(),
        notifications.clone(),
        audit.clone(),
        hasher.clone(),
        ids.clone(),
        clock.clone(),
        providers.clone(),
        config.notice_version.clone(),
        config.verification_otp_digits,
        config.verification_ttl_secs,
    ));
    let submit_tenant = Arc::new(SubmitTenantRequest::new((*submit_dsr).clone()));
    let verify = Arc::new(VerifyRequest::new(
        tx_manager.clone(),
        requests.clone(),
        challenges.clone(),
        participants.clone(),
        hasher.clone(),
        clock.clone(),
        providers.clone(),
        audit.clone(),
        config.verification_max_attempts,
    ));
    let get_status = Arc::new(GetRequestStatus::new(
        tx_manager.clone(),
        requests.clone(),
        appeals.clone(),
        challenges.clone(),
        hasher.clone(),
    ));
    let list_requests = Arc::new(ListRequestsOperator::new(
        tx_manager.clone(),
        requests.clone(),
    ));
    let extend = Arc::new(ExtendDeadline::new(
        tx_manager.clone(),
        requests.clone(),
        jurisdictions.clone(),
        clock.clone(),
        audit.clone(),
    ));
    let reject = Arc::new(PrivacyRejectRequest::new(
        tx_manager.clone(),
        requests.clone(),
        events.clone(),
        clock.clone(),
        audit.clone(),
    ));
    let patch = Arc::new(PatchRequest::new(
        tx_manager.clone(),
        requests.clone(),
        providers.clone(),
        audit.clone(),
    ));
    let appeal = Arc::new(AppealRequest::new(
        tx_manager.clone(),
        requests.clone(),
        appeals.clone(),
        jurisdictions.clone(),
        events.clone(),
        audit.clone(),
        ids.clone(),
        clock.clone(),
    ));
    let decide_appeal = Arc::new(DecideAppeal::new(
        tx_manager.clone(),
        requests.clone(),
        appeals.clone(),
        events.clone(),
        audit.clone(),
        clock.clone(),
    ));
    let list_appeals = Arc::new(ListOpenAppeals::new(tx_manager.clone(), appeals.clone()));
    let get_directives = Arc::new(GetDirectives::new(
        resolver.clone(),
        signals.clone(),
        subjects.clone(),
        tx_manager.clone(),
    ));
    let record_consent = Arc::new(RecordConsentChange::new(
        tx_manager.clone(),
        subjects.clone(),
        jurisdictions.clone(),
        consents.clone(),
        directives.clone(),
        events.clone(),
        clock.clone(),
    ));
    let record_opt_out = Arc::new(RecordOptOut::new(
        tx_manager.clone(),
        subjects.clone(),
        jurisdictions.clone(),
        consents.clone(),
        signals.clone(),
        directives.clone(),
        events.clone(),
        clock.clone(),
        config.notice_version.clone(),
    ));
    let limit_sensitive = Arc::new(RecordLimitSensitive::new((*record_opt_out).clone()));
    let download = Arc::new(DownloadExport::new(
        tx_manager.clone(),
        requests.clone(),
        artifacts.clone(),
        hasher.clone(),
        clock.clone(),
    ));
    let list_jurisdictions = Arc::new(ListJurisdictions::new(
        tx_manager.clone(),
        jurisdictions.clone(),
    ));
    let upsert_jurisdiction = Arc::new(UpsertJurisdiction::new(
        tx_manager.clone(),
        jurisdictions.clone(),
        audit.clone(),
        clock.clone(),
    ));
    let apply_hold = Arc::new(ApplyLegalHold::new(
        tx_manager.clone(),
        holds.clone(),
        events.clone(),
        audit.clone(),
        ids.clone(),
        clock.clone(),
    ));
    let release_hold = Arc::new(ReleaseLegalHold::new(
        tx_manager.clone(),
        holds.clone(),
        events.clone(),
        audit.clone(),
    ));
    let list_holds = Arc::new(ListHolds::new(tx_manager.clone(), holds.clone()));
    let record_incident = Arc::new(RecordBreachIncident::new(
        tx_manager.clone(),
        breaches.clone(),
        jurisdictions.clone(),
        calculator,
        events.clone(),
        audit.clone(),
        ids.clone(),
        clock.clone(),
    ));
    let list_incidents = Arc::new(ListIncidents::new(tx_manager.clone(), breaches.clone()));
    let list_obligations = Arc::new(ListObligations::new(tx_manager.clone(), breaches.clone()));
    let mark_notified = Arc::new(MarkBreachNotified::new(
        tx_manager.clone(),
        breaches.clone(),
        audit.clone(),
    ));
    let list_subprocessors = Arc::new(ListSubProcessors::new(
        tx_manager.clone(),
        processors.clone(),
    ));
    let upsert_subprocessor = Arc::new(UpsertSubProcessor::new(
        tx_manager.clone(),
        processors.clone(),
        audit.clone(),
        clock.clone(),
    ));
    let remove_subprocessor = Arc::new(RemoveSubProcessor::new(
        tx_manager.clone(),
        processors.clone(),
        notifier.clone(),
        audit.clone(),
    ));
    let list_activities = Arc::new(ListActivities::new(tx_manager.clone(), activities.clone()));
    let upsert_activity = Arc::new(UpsertActivity::new(
        tx_manager.clone(),
        activities.clone(),
        audit.clone(),
        ids.clone(),
    ));
    let list_assessments = Arc::new(ListAssessments::new(
        tx_manager.clone(),
        assessments.clone(),
    ));
    let upsert_assessment = Arc::new(UpsertAssessment::new(
        tx_manager.clone(),
        assessments.clone(),
        audit.clone(),
        ids.clone(),
    ));
    let list_rules = Arc::new(ListRetentionRules::new(tx_manager.clone(), rules.clone()));
    let upsert_rule = Arc::new(UpsertRetentionRule::new(
        tx_manager.clone(),
        rules.clone(),
        audit.clone(),
    ));
    let public_metrics = Arc::new(PublicMetrics::new(tx_manager.clone(), metrics.clone()));
    let notices = Arc::new(NoticeService::new(
        tx_manager.clone(),
        rules.clone(),
        processors.clone(),
        jurisdictions.clone(),
    ));

    // Worker jobs (the worker builds its own copies from the same config).
    let process_dsr = Arc::new(ProcessDsrBatch::new(
        tx_manager.clone(),
        requests.clone(),
        participants.clone(),
        holds.clone(),
        artifacts.clone(),
        providers.clone(),
        packager,
        crypto.clone(),
        notifications.clone(),
        events.clone(),
        hasher.clone(),
        clock.clone(),
        config.export_dir.clone(),
        config.export_ttl_days,
        config.export_max_downloads,
        config.backup_retention_days,
    ));
    let propagate_opt_outs = Arc::new(PropagateOptOuts::new(
        tx_manager.clone(),
        consents.clone(),
        processors.clone(),
        notifier,
    ));
    let retention_sweep = Arc::new(RetentionSweep::new(
        tx_manager.clone(),
        rules.clone(),
        consents.clone(),
        signals.clone(),
        clock.clone(),
        audit.clone(),
        config.retention_dry_run,
        config.retention_sweep_batch_size,
    ));
    let cleanup_exports = Arc::new(CleanupExpiredExports::new(
        tx_manager.clone(),
        artifacts.clone(),
    ));
    let generate_metrics = Arc::new(GenerateAnnualMetrics::new(
        tx_manager.clone(),
        requests.clone(),
        metrics.clone(),
        clock.clone(),
    ));

    let gpc = GpcSignalMiddleware::new(
        tx_manager.clone(),
        signals.clone(),
        clock.clone(),
        config.device_cookie_name.clone(),
        config.device_cookie_ttl_days,
    );

    let lock: Arc<dyn sanvi_platform_admin::domain::PrivacyLock> =
        Arc::new(DsrPrivacyLock::new(tx_manager.clone(), requests.clone()));

    let state = sanvi_privacy::adapters::http::PrivacyState {
        submit_dsr,
        submit_tenant_request: submit_tenant,
        verify_request: verify,
        get_request_status: get_status,
        list_requests,
        extend_deadline: extend,
        reject_request: reject,
        patch_request: patch,
        appeal_request: appeal,
        decide_appeal,
        list_appeals,
        get_directives,
        record_consent,
        record_opt_out,
        limit_sensitive,
        download_export: download,
        list_jurisdictions,
        upsert_jurisdiction,
        apply_hold,
        release_hold,
        list_holds,
        record_incident,
        list_incidents,
        list_obligations,
        mark_notified,
        list_subprocessors,
        upsert_subprocessor,
        remove_subprocessor,
        list_activities,
        upsert_activity,
        list_assessments,
        upsert_assessment,
        list_retention_rules: list_rules,
        upsert_retention_rule: upsert_rule,
        public_metrics,
        notices,
        directive_resolver: resolver.clone(),
        notice_version: config.notice_version.clone(),
        backup_retention_days: config.backup_retention_days,
    };

    PrivacyWiring {
        state,
        gpc,
        resolver,
        registry: providers,
        process_dsr,
        propagate_opt_outs,
        retention_sweep,
        cleanup_exports,
        generate_metrics,
        lock,
    }
}

/// The billing context's application state.
pub fn build_billing_state(
    pool: sanvi_persistence::PgPool,
    config: &BillingConfig,
    entitlements_repo: Arc<dyn sanvi_access::domain::EntitlementRepository>,
    features_repo: Arc<dyn sanvi_access::domain::FeatureRepository>,
    audit_writer: Arc<dyn AuditWriter>,
    registry: &prometheus::Registry,
) -> sanvi_billing::adapters::http::BillingState {
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone());

    let plans: Arc<dyn PlanRepository> = Arc::new(SqlxPlanRepository::new());
    let prices: Arc<dyn PlanPriceRepository> = Arc::new(SqlxPlanPriceRepository::new());
    let grants: Arc<dyn PlanEntitlementRepository> = Arc::new(SqlxPlanEntitlementRepository::new());
    let subscriptions: Arc<dyn SubscriptionRepository> =
        Arc::new(SqlxSubscriptionRepository::new());
    let invoices: Arc<dyn InvoiceRepository> = Arc::new(SqlxInvoiceRepository::new());
    let webhook_events: Arc<dyn WebhookEventRepository> =
        Arc::new(SqlxWebhookEventRepository::new());
    let ids: Arc<dyn BillingIds> = Arc::new(BillingSystemIdGenerator);
    let clock: Arc<dyn BillingClock> = Arc::new(BillingSystemClock);

    let stripe_metrics = StripeMetrics::new(registry);
    let stripe: Arc<dyn StripeClient> = Arc::new(HttpStripeClient::new(
        config.stripe_api_base.clone(),
        config.stripe_api_key.clone(),
        config.stripe_api_version.clone(),
        Some(stripe_metrics),
    ));
    let tax_gate: Arc<dyn TaxGate> = Arc::new(CachedTaxGate::new(
        stripe.clone(),
        std::time::Duration::from_secs(300),
    ));

    let syncer: Arc<dyn EntitlementSyncer> =
        Arc::new(AccessEntitlementSyncer::new(entitlements_repo));

    sanvi_billing::adapters::http::BillingState {
        list_public_plans: Arc::new(ListPublicPlans::new(
            tx_manager.clone(),
            plans.clone(),
            prices.clone(),
            grants.clone(),
        )),
        create_checkout: Arc::new(CreateCheckoutSession::new(
            tx_manager.clone(),
            plans.clone(),
            prices.clone(),
            subscriptions.clone(),
            stripe.clone(),
            tax_gate,
            audit_writer.clone(),
            clock.clone(),
            config.success_url.clone(),
            config.cancel_url.clone(),
            config.automatic_tax_enabled,
            config.integration_identifier_suffix_len,
        )),
        create_portal: Arc::new(CreatePortalSession::new(
            tx_manager.clone(),
            subscriptions.clone(),
            stripe.clone(),
            audit_writer.clone(),
            clock.clone(),
            config.portal_return_url.clone(),
        )),
        get_subscription: Arc::new(GetSubscription::new(
            tx_manager.clone(),
            subscriptions.clone(),
            plans.clone(),
        )),
        list_invoices: Arc::new(ListInvoices::new(tx_manager.clone(), invoices.clone())),
        list_plans: Arc::new(ListPlans::new(
            tx_manager.clone(),
            plans.clone(),
            prices.clone(),
            grants.clone(),
        )),
        create_plan: Arc::new(CreatePlan::new(
            tx_manager.clone(),
            plans.clone(),
            ids.clone(),
            clock.clone(),
            audit_writer.clone(),
        )),
        update_plan: Arc::new(UpdatePlan::new(
            tx_manager.clone(),
            plans.clone(),
            clock.clone(),
            audit_writer.clone(),
        )),
        add_price: Arc::new(AddPlanPrice::new(
            tx_manager.clone(),
            plans.clone(),
            prices.clone(),
            ids.clone(),
            clock.clone(),
            audit_writer.clone(),
        )),
        set_entitlements: Arc::new(SetPlanEntitlements::new(
            tx_manager.clone(),
            plans.clone(),
            grants.clone(),
            features_repo,
            audit_writer.clone(),
        )),
        apply_override: Arc::new(ApplySubscriptionOverride::new(
            tx_manager.clone(),
            plans.clone(),
            prices.clone(),
            grants.clone(),
            subscriptions.clone(),
            syncer.clone(),
            ids.clone(),
            clock.clone(),
            audit_writer.clone(),
        )),
        revoke_override: Arc::new(RevokeSubscriptionOverride::new(
            tx_manager.clone(),
            subscriptions.clone(),
            syncer,
            clock.clone(),
            audit_writer.clone(),
        )),
        webhook: Arc::new(WebhookReceiver::new(
            tx_manager,
            webhook_events,
            clock,
            config.stripe_webhook_secret.clone(),
            config.webhook_tolerance_secs,
            config.webhook_allowed_clients.clone(),
        )),
    }
}

/// Assemble the actix `App`: middleware stack, routes, OpenAPI, metrics.
///
/// This is the factory `HttpServer` clones per worker; everything captured
/// is cheap to clone (pools, `Arc`s, plain data).
#[allow(clippy::too_many_arguments)]
pub fn build_app(
    config: AppConfig,
    metrics: Arc<MetricsRegistry>,
    system: SystemState,
    tenancy: TenancyState,
    identity: IdentityWiring,
    access: AccessWiring,
    audit: AuditWiring,
    platform_admin: PlatformAdminWiring,
    billing: Option<sanvi_billing::adapters::http::BillingState>,
    privacy: Option<PrivacyWiring>,
    redis: Option<redis::aio::ConnectionManager>,
) -> App<
    impl actix_web::dev::ServiceFactory<
        ServiceRequest,
        Config = (),
        Response = ServiceResponse<tracing_actix_web::StreamSpan<BoxBody>>,
        Error = actix_web::Error,
        InitError = (),
    > + 'static,
> {
    let server_config = config.server.clone();

    let state = AppState {
        build_info: system.build_info.execute(),
        metrics,
        config: config.clone(),
    };

    // The billing context mounts its tenant routes inside the shared tenant
    // scope (a second scope on the same prefix would shadow the first).
    let tenant_scope = web::scope("/api/v1/tenant")
        .configure(tenancy_routes::tenant_scope_routes)
        .configure(identity_routes::tenant_scope_routes);
    let tenant_scope = match &billing {
        Some(_) => tenant_scope.configure(billing_routes::tenant_scope_routes),
        None => tenant_scope,
    };
    let tenant_scope = match &privacy {
        Some(_) => tenant_scope.configure(sanvi_privacy::routes::tenant_scope_routes),
        None => tenant_scope,
    };

    let app = App::new()
        .app_data(web::Data::new(state))
        .app_data(web::Data::new(system))
        .app_data(web::Data::new(tenancy.clone()))
        .app_data(web::Data::new(identity.state.clone()))
        .app_data(web::Data::new(access.state.clone()))
        .app_data(web::Data::new(identity.webhook.as_ref().clone()))
        .app_data(web::Data::new(access.gate.clone()))
        .app_data(web::Data::new(access.feature_gate.clone()))
        .app_data(sanvi_http::FreshAal2Policy {
            max_age_secs: config.platform_admin.fresh_aal2_max_age_secs,
        })
        .app_data(web::Data::new(audit.state.clone()))
        .app_data(web::Data::new(platform_admin.state.clone()))
        .app_data(web::JsonConfig::default().limit(server_config.max_body_bytes))
        .app_data(web::PayloadConfig::default().limit(server_config.max_body_bytes))
        .configure(health_routes)
        .configure(platform_routes)
        .configure(tenancy_routes::platform_routes)
        .service(tenant_scope)
        .configure(tenancy_routes::public_routes)
        .configure(identity_routes::identity_routes)
        .configure(identity_routes::internal_routes)
        .configure(sanvi_access::routes::access_routes)
        .configure(sanvi_audit::routes::audit_routes)
        .configure(platform_admin_routes::platform_admin_routes)
        .service(web::scope("/api/v1/system").configure(api_routes));

    // Phase-04 gate: with the flag off, the billing state is not wired and
    // no billing route is mounted.
    let app = match billing {
        Some(billing_state) => app
            .app_data(web::Data::new(billing_state))
            .configure(billing_routes::public_routes)
            .configure(billing_routes::billing_platform_routes)
            .configure(billing_routes::billing_internal_routes),
        None => app,
    };

    // Phase-05 gate: privacy routes (subject-scoped scope wrapped with the
    // GPC middleware, public disclosures, platform control plane).
    let privacy_wiring = privacy.clone();
    let app = match privacy {
        Some(wiring) => app
            .app_data(web::Data::new(wiring.state))
            .configure(sanvi_privacy::routes::public_routes)
            .configure(sanvi_privacy::routes::privacy_platform_routes)
            .service(
                web::scope("/api/v1/privacy")
                    .wrap(wiring.gpc)
                    .configure(sanvi_privacy::routes::privacy_routes),
            ),
        None => app,
    };

    // The phase-01 feature gate: with the flag off, no resolution middleware
    // and (by the classification below) no tenant-scoped routes either.
    let tenant_middleware = if config.features.tenancy {
        Some(Arc::new(sanvi_http::TenantMiddleware {
            resolver: tenancy.host_resolver.clone(),
            policy: server_config.tenant.clone(),
        }))
    } else {
        None
    };

    // The phase-02 feature gate: with the flag off, no authentication and no
    // permission gate — every route behaves public (matching the tenancy
    // gate's semantics).
    let authn = if config.features.identity {
        Some(Arc::new(AuthnMiddleware {
            verifier: identity.verifier.clone(),
            session_cookie_name: config.identity.session_cookie_name.clone(),
            exempt_prefixes: {
                let mut prefixes = vec![
                    "/healthz".to_string(),
                    "/readyz".to_string(),
                    "/metrics".to_string(),
                    "/openapi.json".to_string(),
                    "/scalar".to_string(),
                    "/api/v1/public".to_string(),
                    "/internal".to_string(),
                ];
                // Self-service privacy endpoints are their own credential:
                // opt-outs may never be gated on authentication.
                if privacy_wiring.is_some() {
                    prefixes.push("/api/v1/privacy".to_string());
                }
                prefixes
            },
        }))
    } else {
        None
    };

    // The phase-03 feature gate: with the flag off, no impersonation
    // resolution (the platform_admin context is not wired).
    let impersonation = if config.features.platform_admin {
        Some(platform_admin.impersonation.clone())
    } else {
        None
    };

    middleware::stack(
        app,
        server_config,
        redis,
        tenant_middleware,
        authn,
        impersonation,
    )
}

/// Platform-level routes that belong to no single context.
fn platform_routes(cfg: &mut web::ServiceConfig) {
    cfg.route("/metrics", web::get().to(metrics))
        .route("/openapi.json", web::get().to(openapi_json));
}

/// `/metrics` handler: Prometheus text format.
async fn metrics(state: web::Data<AppState>) -> HttpResponse {
    match state.metrics.render() {
        Ok(body) => HttpResponse::Ok()
            .content_type("text/plain; version=0.0.4")
            .body(body),
        Err(err) => {
            tracing::error!(error = %err, "failed to render metrics");
            HttpResponse::InternalServerError().json(ProblemDetail {
                r#type: format!("{}/internal-error", sanvi_http::error::PROBLEM_TYPE_BASE),
                title: "Internal Server Error".to_string(),
                status: 500,
                detail: "failed to render metrics".to_string(),
                instance: Some("/metrics".to_string()),
                trace_id: None,
                reason: None,
            })
        }
    }
}

/// `/openapi.json` handler: the merged contract document.
async fn openapi_json() -> HttpResponse {
    use utoipa::OpenApi as _;
    let doc = ApiDoc::openapi();
    HttpResponse::Ok()
        .content_type("application/json")
        .body(doc.to_json().expect("openapi serialization is infallible"))
}
