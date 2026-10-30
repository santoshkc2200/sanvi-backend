//! `sanvi-api` — the composition root for the HTTP application.
//!
//! `lib.rs` exposes [`AppConfig`], [`build_app`] and the merged OpenAPI
//! document so tests (and the CLI's contract check) can reuse them;
//! `main.rs` is a thin binary: read config → init telemetry → connect
//! infrastructure → inject adapters into use cases → serve.

pub mod accounting;
pub mod advertising;
pub mod compliance;
pub mod config;
pub mod domains;
pub mod openapi;
pub mod payments;
pub mod product_access;

use std::future::Future;
use std::sync::Arc;

use actix_web::{
    App, HttpRequest, HttpResponse,
    body::BoxBody,
    dev::{ServiceRequest, ServiceResponse},
    http::header,
    web,
};
use chrono::{DateTime, Utc};
use sanvi_access::{
    AccessConfig,
    adapters::{
        HttpKetoClient, InMemoryEntitlementCache, OutboxEventPublisher, RedisEntitlementCache,
        SqlxEntitlementRepository, SqlxFeatureRepository, SqlxMembershipSource,
        SqlxQuotaUsageStore, SqlxRoleRepository,
    },
    application::entitlements::EntitlementPolicy,
    application::{
        CachedAuthorizer, CheckPermission, CreateCustomRole, DefineFeature, DegradedAuthorizer,
        DeleteCustomRole, DeprecateFeature, EntitlementFeatureGate, EntitlementResolver,
        ExpireEntitlements, GrantEntitlement, GrantEntitlementOutput, ListFeatures,
        ListPermissions, ListRoles, ListTenantEntitlements, ListTenantOverrides,
        RebuildAccessTuples, RevokeEntitlement, SeedFeatures, UpdateCustomRole, UpdateFeature,
    },
    domain::{
        ApprovalGate, Authorizer, DecisionCache, EntitlementCache, EntitlementRepository,
        EventPublisher as AccessEventPublisher, FeatureGate, FeatureRepository, PermissionStore,
        QuotaUsageStore, RoleRepository,
    },
};
use sanvi_advertising::routes as advertising_routes;
use sanvi_audit::{
    AuditConfig,
    adapters::{SqlxAuditReader, SqlxAuditWriter},
    application::{ListAuditLog, VerifyAuditChain},
    domain::{AuditReader, AuditWriter},
};
use sanvi_billing::{
    BillingConfig,
    adapters::{
        AccessEntitlementSyncer, CachedTaxGate, HttpStripeClient, SqlxCheckoutStateRepository,
        SqlxInvoiceRepository, SqlxPlanEntitlementRepository, SqlxPlanPriceRepository,
        SqlxPlanRepository, SqlxSubscriptionRepository, SqlxWebhookEventRepository, StripeMetrics,
        SystemClock as BillingSystemClock, SystemIdGenerator as BillingSystemIdGenerator,
    },
    application::{
        AddPlanPrice, ApplySubscriptionOverride, CreateCheckoutSession, CreatePlan,
        CreatePortalSession, GetSubscription, ListInvoices, ListPlans, ListPublicPlans,
        RevokeSubscriptionOverride, SetPlanEntitlements, UpdatePlan, WebhookReceiver,
    },
    domain::{
        CheckoutStateRepository, Clock as BillingClock, EntitlementSyncer,
        IdGenerator as BillingIds, InvoiceRepository, PlanEntitlementRepository,
        PlanPriceRepository, PlanRepository, StripeClient, SubscriptionRepository, TaxGate,
        WebhookEventRepository,
    },
    routes as billing_routes,
};
use sanvi_domains::{
    DomainsConfig,
    adapters::{
        DomainsMetrics, LiveDomainCache, OutboxEventPublisher as DomainsEvents,
        SqlxCertificateRepository, SqlxCustomDomainRepository, SqlxDnsProvider,
        SqlxDnsRecordRepository, SqlxDomainOrderRepository, SystemClock as DomainsClock,
        build_certificate_authority, build_domain_registrar, build_purchase_charger,
    },
    application::{
        AuthorizeTls, ClaimCustomDomain, GetDomainInstructions, ListCustomDomains,
        ListDomainOrders, LiveDomainLookupService, PlaceDomainOrder, PlatformListDomains,
        PlatformReleaseDomain, PromotePrimaryDomain, RemoveCustomDomain, RequestVerification,
        SearchDomains, SetOrderAutoRenew,
    },
    domain::{
        CustomDomainRepository, DomainFeatureGate, DomainOrderRepository, DomainsError,
        EventPublisher as DomainsEventPublisher, GuideTranslator, TenantState,
    },
    routes as domains_routes,
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
        EventPublisher, IdentityError, InvitationRepository, KratosClient, LinkIntentRepository,
        LinkedAccountRepository, MembershipRepository, Notifications, RoleCatalog, SessionCache,
        TenantCatalog, TenantSummary as IdentityTenantSummary, UserRepository,
    },
    routes as identity_routes,
};
use sanvi_kernel::UserId;
use sanvi_localization::{
    LocalizationConfig,
    adapters::{
        LocaleResolverService, LocalizationCache, LocalizationMetrics, LocalizationService,
        NotificationLocalizer, OutboxEventPublisher as LocalizationEvents, PoolUserLocaleProvider,
        SqlxSupportedLocaleRepository, SqlxTenantLocaleRepository, SqlxTranslationRepository,
        SystemClock as LocalizationClock,
    },
    application::{
        CatalogService, GetLocalizationSettings, GetPlatformTranslations, GetTenantOverrides,
        UpdateLocalizationSettings, UpdatePlatformTranslations, UpdateTenantOverrides,
    },
    domain::{
        Arg as LocalizationArg, Clock as LocalizationClockTrait,
        EventPublisher as LocalizationEventPublisher, SupportedLocaleRepository,
        TenantLocaleRepository, TranslationRepository, Translator as LocalizationTranslator,
    },
    routes as localization_routes,
};
use sanvi_media::routes as media_routes;
use sanvi_notifications::{
    EnqueueEmail, NotificationGatewayPort, build_idempotency_key, secret_fingerprint,
};
use sanvi_payments::routes as payments_routes;
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
        OutboxEventPublisher as PrivacyEvents, PrivacyContextProvider,
        RuleBasedObligationCalculator, Sha256Hasher, SqlxAppealRepository,
        SqlxAuthorizedAgentRepository, SqlxBreachRepository, SqlxConsentRepository,
        SqlxDirectiveRepository, SqlxDsrRepository, SqlxEnvelopeCrypto,
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
        SlugReservationPort, TenantId, TenantRepository, TenantResolutionCache,
        TenantSettingsRepository,
    },
    routes as tenancy_routes,
};
use sanvi_theming::{
    ThemingConfig,
    adapters::{
        HostTenantResolverAdapter, LocalFileStore, OutboxEventPublisher as ThemingEvents,
        ResolvedThemeCache, SqlxTenantThemeRepository, SqlxThemeRepository,
        SystemClock as ThemingClock, ThemingMetrics,
    },
    application::{
        InstallThemeVersion, ListAvailableThemes, ListThemeVersionsAdmin, ListThemesAdmin,
        PreviewTenantTheme, PublishTenantTheme, RemoveBrandAsset, ResolvePublicTheme,
        ResolveThemeService, RollbackTenantTheme, UpdateTenantThemeDraft, UploadBrandAsset,
        draft::GetTenantThemeDraft,
    },
    domain::{
        BrandAssetStore, Clock as ThemingClockTrait, EventPublisher as ThemingEventPublisher,
        TenantContextResolver, TenantThemeRepository, ThemeFeatureGate, ThemeRepository,
    },
    routes as theming_routes,
};

pub use config::AppConfig;
pub use openapi::ApiDoc;

struct QuotaPlanResolver(Arc<sanvi_http::quotas::QuotaService>);
impl sanvi_http::api_limits::PlanResolver for QuotaPlanResolver {
    fn plan<'a>(
        &'a self,
        tenant: sanvi_kernel::TenantId,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<sanvi_http::api_limits::LimitPlan, sanvi_http::ApiError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            use sanvi_http::api_limits::LimitPlan;
            match self
                .0
                .plan_for(tenant)
                .await
                .map_err(sanvi_http::ApiError::from)?
                .tier
                .as_str()
            {
                "starter" => Ok(LimitPlan::Starter),
                "professional" => Ok(LimitPlan::Professional),
                "enterprise" => Ok(LimitPlan::Enterprise),
                _ => Err(sanvi_http::ApiError::service_unavailable(
                    "unknown billing plan",
                )),
            }
        })
    }
}

/// State shared by every handler in the app.
#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub build_info: BuildInfo,
    pub metrics: Arc<MetricsRegistry>,
}

/// Build the system context's application state (use cases + probes).
/// `environment` names the deployment the process runs in (FR-1102's build
/// probe reports it).
pub fn build_system_state(
    build_info: BuildInfo,
    environment: &str,
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
        build_details: Arc::new(sanvi_system::application::GetBuildDetails::new(
            &build_info,
            environment.to_string(),
        )),
        build_info: Arc::new(GetBuildInfo::new(build_info.clone())),
        readiness: Arc::new(CheckReadiness::new(
            probes,
            std::time::Duration::from_secs(readiness_budget_secs),
        )),
        liveness: Arc::new(CheckLiveness::new()),
    }
}

/// Build the tenancy context's application state: real adapters wired into
/// every use case, plus the resolver the middleware stack consumes. The
/// phase-08 custom-domain lookup hook is optional (off when the domains
/// feature flag is off).
pub fn build_tenancy_state(
    pool: sanvi_persistence::PgPool,
    ids: std::sync::Arc<dyn sanvi_kernel::RawIdGenerator>,
    redis: Option<redis::aio::ConnectionManager>,
    config: &TenancyConfig,
    custom_domains: Option<Arc<dyn sanvi_tenancy::adapters::resolution::CustomDomainLookup>>,
) -> TenancyState {
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let tenants: Arc<dyn TenantRepository> = Arc::new(SqlxTenantRepository::new());
    let settings: Arc<dyn TenantSettingsRepository> = Arc::new(SqlxTenantSettingsRepository::new());
    let slugs: Arc<dyn SlugReservationPort> = Arc::new(SqlxSlugReservationPort::new());
    let events: Arc<dyn TenancyEventPublisherTrait> =
        Arc::new(TenancyEventPublisher::new(ids.clone()));
    let ids: Arc<dyn TenancyIdGeneratorTrait> = Arc::new(TenancyIdGenerator::new(ids));
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
    let mut host_resolver = HostTenantResolver::new(
        resolve.clone(),
        config.base_domain.clone(),
        config.usable_keys(),
    );
    if let Some(lookup) = custom_domains {
        host_resolver = host_resolver.with_custom_domains(lookup);
    }
    let host_resolver = Arc::new(host_resolver);

    TenancyState {
        quotas: Some(Arc::new(sanvi_http::quotas::QuotaService::new(
            tx_manager.clone(),
            Arc::new(sanvi_http::quotas::PgQuotaPlanProvider::new(
                tx_manager.clone(),
            )),
            redis.clone(),
        ))),
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

/// Invitation mail: rendered here, delivered by the notification service.
///
/// This is Lane B — a direct, post-commit send rather than an outbox-driven
/// one, because `invite_url` embeds the raw invitation token. With no gateway
/// configured it falls back to logging so a dev machine still boots; it never
/// logs the address, the token or the body.
struct LocalizedIdentityNotifications {
    localizer: Arc<NotificationLocalizer>,
    gateway: Option<Arc<dyn NotificationGatewayPort>>,
}

#[async_trait::async_trait]
impl Notifications for LocalizedIdentityNotifications {
    async fn send_invitation(
        &self,
        tenant_id: TenantId,
        invitation_id: sanvi_identity::domain::InvitationId,
        email: &sanvi_identity::domain::Email,
        tenant_name: &str,
        invite_url: &str,
    ) -> Result<(), IdentityError> {
        let context = self.localizer.tenant_locale(tenant_id).await;
        let subject = self.localizer.render(
            &context,
            "emails.invitation.subject",
            &[("tenant", LocalizationArg::from(tenant_name))],
        );
        let body = self.localizer.render(
            &context,
            "emails.invitation.body",
            &[
                ("name", LocalizationArg::from(email.as_str())),
                ("tenant", LocalizationArg::from(tenant_name)),
                ("url", LocalizationArg::from(invite_url)),
                ("days", LocalizationArg::from(7i64)),
            ],
        );

        let Some(gateway) = &self.gateway else {
            tracing::info!(
                tenant_id = %tenant_id,
                invitation_id = %invitation_id,
                locale = %context.locale,
                notification = "invitation",
                "notification service not configured; invitation not delivered"
            );
            return Ok(());
        };

        // TASK-027 (FR-1127): the HTML alternative derives from the same
        // rendered text; the URL line becomes a link labelled with the
        // locale's action text, never a naked URL.
        let action_label = self
            .localizer
            .render(&context, "emails.invitation.action", &[]);
        let html = sanvi_localization::adapters::email_html::email_html(
            &context.locale,
            &subject,
            &body,
            Some(&action_label),
        );
        let tenant = tenant_id.to_string();
        let key = build_idempotency_key(
            "identity",
            "invitation",
            &invitation_id.to_string(),
            &secret_fingerprint(invite_url),
        );
        gateway
            .enqueue_email(EnqueueEmail {
                tenant_id: Some(&tenant),
                recipient: email.as_str(),
                subject: &subject,
                text_body: &body,
                html_body: Some(&html),
                idempotency_key: &key,
            })
            .await
            .map_err(|err| IdentityError::NotificationUnavailable(err.to_string()))?;
        Ok(())
    }
}

/// External repository, catalog, and localizer dependencies for identity state.
pub struct IdentityDependencies {
    pub role_catalog: Arc<dyn RoleCatalog>,
    pub tenant_catalog: Arc<dyn TenantCatalog>,
    pub notification_localizer: Option<Arc<NotificationLocalizer>>,
    /// Delivery for Lane B invitation mail. `None` leaves the adapter logging
    /// only, which is what a dev machine without the service configured gets.
    pub notification_gateway: Option<Arc<dyn NotificationGatewayPort>>,
}

/// Build the identity context's application state.
pub fn build_identity_state(
    pool: sanvi_persistence::PgPool,
    ids: std::sync::Arc<dyn sanvi_kernel::RawIdGenerator>,
    redis: Option<redis::aio::ConnectionManager>,
    config: &IdentityConfig,
    kratos_public_url: &str,
    kratos_admin_url: &str,
    deps: IdentityDependencies,
) -> IdentityWiring {
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let users: Arc<dyn UserRepository> = Arc::new(SqlxUserRepository::new());
    let memberships: Arc<dyn MembershipRepository> = Arc::new(SqlxMembershipRepository::new());
    let linked: Arc<dyn LinkedAccountRepository> = Arc::new(SqlxLinkedAccountRepository::new());
    let invitations: Arc<dyn InvitationRepository> = Arc::new(SqlxInvitationRepository::new());
    let link_intents: Arc<dyn LinkIntentRepository> = Arc::new(SqlxLinkIntentRepository::new());
    let events: Arc<dyn EventPublisher> = Arc::new(IdentityEventPublisher::new(ids.clone()));
    let notifications: Arc<dyn Notifications> = match deps.notification_localizer {
        Some(localizer) => Arc::new(LocalizedIdentityNotifications {
            localizer,
            gateway: deps.notification_gateway,
        }),
        None => Arc::new(LoggingNotifications::new()),
    };
    let ids: Arc<dyn sanvi_identity::domain::IdGenerator> = Arc::new(SystemIdGenerator::new(ids));
    let clock: Arc<dyn sanvi_identity::domain::Clock> = Arc::new(SystemClock);
    let role_catalog = deps.role_catalog;
    let kratos: Arc<dyn KratosClient> = Arc::new(
        HttpKratosClient::new(
            kratos_public_url.to_string(),
            Some(kratos_admin_url.to_string()),
            config.session_cookie_name.clone(),
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
        get_me: Arc::new(GetMe::new(
            tx_manager.clone(),
            users.clone(),
            memberships.clone(),
            role_catalog.clone(),
            deps.tenant_catalog,
        )),
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
            memberships.clone(),
            role_catalog.clone(),
            events.clone(),
            notifications.clone(),
            ids,
            clock.clone(),
            config.invitation_ttl_days,
            config.app_base_url.clone(),
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
            config.app_base_url.clone(),
        )),
        revoke_invitation: Arc::new(RevokeInvitation::new(tx_manager.clone(), invitations)),
        link_intent_ttl_secs: config.link_intent_ttl_secs,
    };

    let webhook = Arc::new(sanvi_identity::adapters::http::WebhookState {
        sync,
        logout,
        secret: config.webhook_secret.expose_secret().clone(),
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
    /// The Keto-outage rule (TASK-023): wraps `authorizer`.
    pub degraded_authorizer: Arc<DegradedAuthorizer>,
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
    authorizer: Arc<DegradedAuthorizer>,
}

impl PermissionGate for AccessGate {
    fn check<'a>(
        &'a self,
        subject: &'a Subject,
        permission: &'a str,
        resource: &'a ResourceRef,
        write: bool,
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
            // TASK-023: the degraded authorizer owns the outage rule —
            // writes deny by default, cached reads survive the grace
            // window — so a Keto outage denies (403) instead of inviting a
            // 503 retry storm against a dependency that cannot answer.
            match self
                .authorizer
                .check_with_mode(subject.user_id, &permission, &resource, write)
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
        tenant_id: TenantId,
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
    ids: std::sync::Arc<dyn sanvi_kernel::RawIdGenerator>,
    config: &AccessConfig,
    keto_read_url: &str,
    keto_write_url: &str,
    memberships: Arc<dyn MembershipRepository>,
    audit_writer: Arc<dyn AuditWriter>,
    approval_gate: Arc<dyn ApprovalGate>,
    registry: &prometheus::Registry,
    redis: Option<redis::aio::ConnectionManager>,
    degradation: std::sync::Arc<sanvi_resilience::DegradationRegistry>,
) -> AccessWiring {
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let roles: Arc<dyn RoleRepository> = Arc::new(SqlxRoleRepository::new());
    let store: Arc<dyn PermissionStore> = Arc::new(
        HttpKetoClient::with_resilience(
            keto_read_url.to_string(),
            keto_write_url.to_string(),
            std::time::Duration::from_secs(5),
            sanvi_resilience::BreakerConfig::default(),
            Some(degradation.clone()),
        )
        .expect("keto client construction is infallible"),
    );
    // A process-local decision cache is unsafe here: the Keto sync consumer
    // runs in the worker, so its `invalidate_user` can never reach an API
    // replica's own map, and a demoted member keeps their old permissions
    // until each replica's TTL lapses independently. Without Redis we cache
    // nothing rather than cache staleness — slower, never wrong.
    let cache: Arc<dyn DecisionCache> = match &redis {
        Some(conn) => Arc::new(sanvi_access::adapters::RedisDecisionCache::new(
            conn.clone(),
        )),
        None => {
            tracing::warn!(
                "no Redis connection: permission decisions will not be cached. \
                 Authorization stays correct but every check reaches Keto."
            );
            Arc::new(sanvi_access::adapters::NoopDecisionCache)
        }
    };
    let features: Arc<dyn FeatureRepository> = Arc::new(SqlxFeatureRepository::new());
    let entitlements: Arc<dyn EntitlementRepository> = Arc::new(SqlxEntitlementRepository::new());
    let quota: Arc<dyn QuotaUsageStore> = Arc::new(SqlxQuotaUsageStore::new());
    let events: Arc<dyn AccessEventPublisher> = Arc::new(OutboxEventPublisher::new(ids.clone()));
    let clock: Arc<dyn sanvi_access::domain::Clock> = Arc::new(sanvi_access::adapters::SystemClock);
    let ids: Arc<dyn sanvi_access::domain::IdGenerator> =
        Arc::new(sanvi_access::adapters::SystemRoleIdGenerator::new(ids));

    let check = Arc::new(CheckPermission::new(
        tx_manager.clone(),
        roles.clone(),
        store.clone(),
        Arc::new(sanvi_access::adapters::SqlxTenantRoleSource::new(
            tx_manager.clone(),
            memberships.clone(),
        )),
    ));
    let cached_authorizer: Arc<dyn Authorizer> = Arc::new(CachedAuthorizer::new(
        check,
        cache.clone(),
        config.decision_cache_ttl_secs,
    ));
    // TASK-023: the Keto-outage rule wraps the cached authorizer; the
    // registry is the shared one the degraded-mode header reads.
    let degraded_authorizer = Arc::new(DegradedAuthorizer::new(
        cached_authorizer.clone(),
        cache.clone(),
        degradation.clone(),
    ));
    let authorizer: Arc<dyn Authorizer> = degraded_authorizer.clone();
    let gate: Arc<dyn PermissionGate> = Arc::new(AccessGate {
        authorizer: degraded_authorizer.clone(),
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
        Arc::new(sanvi_cache::SingleFlight::new()),
        Arc::new(sanvi_cache::CacheMetrics::new(registry)),
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
        degraded_authorizer,
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

/// Build the audit context's application state. `archival_enabled` is
/// the `platform.archival` feature flag (TASK-028): when on, ranged
/// reads consult `platform.archives` and refuse archived windows.
pub fn build_audit_state(
    pool: sanvi_persistence::PgPool,
    ids: std::sync::Arc<dyn sanvi_kernel::RawIdGenerator>,
    config: &AuditConfig,
    archival_enabled: bool,
) -> AuditWiring {
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let writer: Arc<dyn AuditWriter> = Arc::new(SqlxAuditWriter::new(ids.clone()));
    let reader: Arc<dyn AuditReader> = Arc::new(SqlxAuditReader::new());
    let mut list = ListAuditLog::new(tx_manager.clone(), reader.clone());
    if archival_enabled {
        list = list.with_archive_check(Arc::new(
            sanvi_audit::adapters::SqlxArchivedRangeCheck::new(),
        ));
    }
    let list = Arc::new(list);
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
        actor_id: UserId,
        tenant_id: TenantId,
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
                    approval_id: sanvi_platform_admin::domain::ApprovalId::from_snowflake(
                        approval_id,
                    ),
                },
            ),
            Err(err) => Err(map_access_err(err)),
        }
    }

    async fn grant_override_approved(
        &self,
        requested_by: UserId,
        tenant_id: TenantId,
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
        actor_id: UserId,
        tenant_id: TenantId,
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
        tenant_id: TenantId,
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

/// External repository and adapter dependencies for platform admin state.
pub struct PlatformAdminDependencies {
    pub tenants: Arc<dyn TenantRepository>,
    pub memberships: Arc<dyn MembershipRepository>,
    pub entitlements: Arc<dyn EntitlementRepository>,
    pub entitlement_admin: Arc<dyn EntitlementAdmin>,
    pub audit_writer: Arc<dyn AuditWriter>,
    pub privacy_lock: Arc<dyn PrivacyLock>,
}

/// The `OffboardingAdmin` port over the tenancy context's use cases
/// (TASK-028, FR-1131): the record is tenant-lifecycle data, the
/// operator surface is the platform control plane.
struct TenancyOffboardingAdmin {
    request: Arc<sanvi_tenancy::application::RequestOffboarding>,
    get: Arc<sanvi_tenancy::application::GetOffboarding>,
}

fn map_tenancy_offboarding_err(
    err: sanvi_tenancy::domain::TenancyError,
) -> sanvi_platform_admin::domain::PlatformAdminError {
    match err {
        sanvi_tenancy::domain::TenancyError::TenantNotFound { id } => {
            sanvi_platform_admin::domain::PlatformAdminError::TenantNotFound {
                id: parse_tenant_id(&id),
            }
        }
        sanvi_tenancy::domain::TenancyError::OffboardingExportMissing { request_id } => {
            sanvi_platform_admin::domain::PlatformAdminError::OffboardingExportMissing(request_id)
        }
        other => {
            sanvi_platform_admin::domain::PlatformAdminError::OffboardingConflict(other.to_string())
        }
    }
}

fn parse_tenant_id(raw: &str) -> sanvi_kernel::TenantId {
    // The raw string came from a TenantId in the first place; the parse
    // only re-crosses the transport boundary. 1 is the placeholder a
    // malformed value can never be in practice.
    raw.parse().unwrap_or_else(|_| {
        sanvi_kernel::TenantId::from_snowflake(
            sanvi_kernel::SnowflakeId::try_from(1).expect("1 is a valid snowflake"),
        )
    })
}

#[async_trait::async_trait]
impl sanvi_platform_admin::domain::OffboardingAdmin for TenancyOffboardingAdmin {
    async fn request(
        &self,
        tenant_id: sanvi_kernel::TenantId,
        idempotency_key: String,
        requested_by: Option<sanvi_kernel::UserId>,
    ) -> Result<
        sanvi_platform_admin::domain::OffboardingSnapshot,
        sanvi_platform_admin::domain::PlatformAdminError,
    > {
        let request = self
            .request
            .execute(tenant_id, idempotency_key, requested_by)
            .await
            .map_err(map_tenancy_offboarding_err)?;
        Ok(to_offboarding_snapshot(request))
    }

    async fn get(
        &self,
        tenant_id: sanvi_kernel::TenantId,
    ) -> Result<
        Option<sanvi_platform_admin::domain::OffboardingSnapshot>,
        sanvi_platform_admin::domain::PlatformAdminError,
    > {
        let found = self
            .get
            .execute(tenant_id)
            .await
            .map_err(map_tenancy_offboarding_err)?;
        Ok(found.map(to_offboarding_snapshot))
    }
}

fn to_offboarding_snapshot(
    request: sanvi_tenancy::domain::OffboardingRequest,
) -> sanvi_platform_admin::domain::OffboardingSnapshot {
    use sanvi_platform_admin::domain::{OffboardingExportEvidence, OffboardingSnapshot};
    OffboardingSnapshot {
        id: request.id,
        tenant_id: request.tenant_id.as_i64(),
        idempotency_key: request.idempotency_key,
        status: request.status.as_str().to_string(),
        export: request.export.map(|export| OffboardingExportEvidence {
            path: export.path,
            checksum: export.checksum,
        }),
        purge_report: request.purge_report,
        error: request.error,
        created_at: request.created_at,
        updated_at: request.updated_at,
        completed_at: request.completed_at,
    }
}

/// Build the platform_admin context's application state. `entitlement_admin`
/// is the composition root's adapter over the access context's use cases;
/// the returned `approval_gate` is the same adapter the access context's
/// `GrantEntitlement` consumes.
pub fn build_platform_admin_state(
    pool: sanvi_persistence::PgPool,
    ids: std::sync::Arc<dyn sanvi_kernel::RawIdGenerator>,
    config: &PlatformAdminConfig,
    deps: PlatformAdminDependencies,
) -> PlatformAdminWiring {
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let grants: Arc<dyn ImpersonationGrantRepository> =
        Arc::new(SqlxImpersonationGrantRepository::new());
    let approvals: Arc<dyn PendingApprovalRepository> =
        Arc::new(SqlxPendingApprovalRepository::new());
    let clock: Arc<dyn PlatformAdminClockTrait> = Arc::new(PlatformAdminClock);
    let events: Arc<dyn PlatformAdminEventsTrait> = Arc::new(PlatformAdminEvents::new(ids.clone()));
    let raw_ids = ids.clone();
    let ids: Arc<dyn PlatformAdminIdsTrait> = Arc::new(PlatformAdminIds::new(ids));

    let views: Arc<dyn TenantAdminViewRepository> = Arc::new(SqlxTenantAdminViewRepository::new(
        deps.tenants,
        deps.memberships.clone(),
        deps.entitlements,
        grants.clone(),
        clock.clone(),
    ));

    let create_impersonation = Arc::new(CreateImpersonation::new(
        tx_manager.clone(),
        grants.clone(),
        deps.memberships.clone(),
        deps.privacy_lock,
        events.clone(),
        ids.clone(),
        clock.clone(),
        deps.audit_writer.clone(),
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
        deps.audit_writer.clone(),
    ));

    let record_approval = Arc::new(RecordApprovalRequest::new(
        approvals.clone(),
        ids.clone(),
        clock.clone(),
    ));
    let decide = Arc::new(DecideApproval::new(
        tx_manager.clone(),
        approvals.clone(),
        deps.entitlement_admin.clone(),
        clock.clone(),
        deps.audit_writer.clone(),
    ));

    let verifier = Arc::new(SqlxImpersonationVerifier::new(
        grants.clone(),
        deps.memberships,
        tx_manager.clone(),
        clock,
    ));
    let impersonation = Arc::new(sanvi_http::ImpersonationMiddleware { verifier });

    // TASK-028 (FR-1131): the offboarding surface over the tenancy use
    // cases — same repositories and adapters the tenancy context uses.
    let tenancy_clock: Arc<dyn sanvi_tenancy::domain::Clock> = Arc::new(TenancyClock);
    let offboarding_repo: Arc<dyn sanvi_tenancy::domain::OffboardingRepository> =
        Arc::new(sanvi_tenancy::adapters::repo::SqlxOffboardingRepository::new());
    let offboarding = Arc::new(TenancyOffboardingAdmin {
        request: Arc::new(sanvi_tenancy::application::RequestOffboarding::new(
            tx_manager.clone(),
            Arc::new(SqlxTenantRepository::new()),
            offboarding_repo.clone(),
            tenancy_clock,
            Arc::new(TenancyIdGenerator::new(raw_ids.clone())),
        )),
        get: Arc::new(sanvi_tenancy::application::GetOffboarding::new(
            tx_manager.clone(),
            offboarding_repo,
        )),
    });

    let state = sanvi_platform_admin::adapters::http::PlatformAdminState {
        search_tenants: Arc::new(SearchTenantAdminViews::new(
            tx_manager.clone(),
            views.clone(),
        )),
        get_tenant: Arc::new(GetTenantAdminView::new(tx_manager.clone(), views)),
        create_impersonation,
        list_impersonations,
        revoke_impersonation,
        entitlements: deps.entitlement_admin,
        list_approvals: Arc::new(ListPendingApprovals::new(
            tx_manager.clone(),
            approvals.clone(),
        )),
        approve: Arc::new(ApproveRequest::new(decide.clone())),
        reject: Arc::new(RejectRequest::new(decide)),
        offboarding: Some(offboarding),
        fees: Some(Arc::new(
            sanvi_platform_admin::adapters::SqlxFeeRepository::new(
                tx_manager.clone(),
                deps.audit_writer,
            ),
        )),
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

/// DSR verification codes and export credentials.
///
/// Lane B, for the same reason as invitations: both payloads are live
/// credentials and must not travel through the outbox. Neither the code, the
/// passphrase, the token nor the recipient is ever logged.
struct LocalizedPrivacyNotifications {
    localizer: Arc<NotificationLocalizer>,
    gateway: Option<Arc<dyn NotificationGatewayPort>>,
}

#[async_trait::async_trait]
impl PrivacyNotifications for LocalizedPrivacyNotifications {
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

        let html = sanvi_localization::adapters::email_html::email_html(
            &context.locale,
            &subject,
            &body,
            None,
        );
        let tenant = subject_ref.tenant_id.map(|id| id.to_string());
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
        tenant_id: TenantId,
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

/// Everything the shared provider-registry builder needs from the
/// environment. The CLI and the compliance tests pass local or unconfigured
/// values for the external systems: a provider only calls Kratos/Keto/
/// Stripe when the erased subject actually has rows in its context, so a
/// drill subject with data only in the privacy ledger never touches them.
pub struct PrivacyProviderDeps {
    pub accounting_service: sanvi_accounting::adapters::remote::ServiceConfig,
    pub domains_service: sanvi_domains::adapters::remote::ServiceConfig,
    pub payments_service: sanvi_payments::adapters::remote::ServiceConfig,
    pub advertising_service: sanvi_advertising::adapters::remote::ServiceConfig,
    pub media: sanvi_media::MediaConfig,
    pub product_access: sanvi_product_access::ProductAccessConfig,
    pub tx: sanvi_persistence::TxManager,
    pub kratos_public_url: String,
    pub kratos_admin_url: String,
    pub keto_read_url: String,
    pub keto_write_url: String,
    pub session_cookie_name: String,
    pub stripe_api_base: String,
    pub stripe_api_key: sanvi_telemetry::Redacted<String>,
    pub stripe_api_version: String,
    pub audit_writer: Arc<dyn AuditWriter>,
    pub privacy_lock: Arc<dyn sanvi_platform_admin::domain::PrivacyLock>,
}

/// The phase-05 provider registry, built once here so the API binary, the
/// CLI's compliance drills and the compliance test crate cannot drift: a
/// context that quietly never joined fails `build_privacy_state`'s
/// registry assert at the same call site for every consumer.
pub fn privacy_provider_registry(deps: &PrivacyProviderDeps) -> ProviderRegistry {
    let tx = &deps.tx;
    vec![
        Arc::new(sanvi_system::adapters::SystemPrivacyProvider::new()),
        Arc::new(
            sanvi_product_access::adapters::ProductAccessPrivacyProvider::from_config(
                &deps.product_access,
            )
            .expect("product_access privacy provider construction"),
        ),
        Arc::new(
            sanvi_accounting::adapters::privacy_provider::RemoteAccountingPrivacyProvider::new(
                &deps.accounting_service,
            )
            .expect("validated Accounting service configuration"),
        ),
        Arc::new(
            sanvi_tenancy::adapters::privacy_provider::TenancyPrivacyProvider::new(
                tx.clone(),
                Arc::new(sanvi_tenancy::adapters::repo::SqlxTenantRepository::new()),
            ),
        ),
        Arc::new(
            sanvi_identity::adapters::privacy_provider::IdentityPrivacyProvider::new(
                tx.clone(),
                Arc::new(sanvi_identity::adapters::repo::SqlxUserRepository::new()),
                Arc::new(sanvi_identity::adapters::repo::SqlxMembershipRepository::new()),
                Arc::new(sanvi_identity::adapters::repo::SqlxLinkedAccountRepository::new()),
                Arc::new(sanvi_identity::adapters::repo::SqlxInvitationRepository::new()),
                Arc::new(sanvi_identity::adapters::repo::SqlxLinkIntentRepository::new()),
                Arc::new(
                    HttpKratosClient::new(
                        deps.kratos_public_url.clone(),
                        Some(deps.kratos_admin_url.clone()),
                        deps.session_cookie_name.clone(),
                    )
                    .expect("kratos client construction is infallible"),
                ),
                Arc::new(sanvi_identity::adapters::SystemClock),
            ),
        ),
        Arc::new(
            sanvi_access::adapters::privacy_provider::AccessPrivacyProvider::new(
                tx.clone(),
                Arc::new(SqlxEntitlementRepository::new()),
                Arc::new(
                    HttpKetoClient::new(deps.keto_read_url.clone(), deps.keto_write_url.clone())
                        .expect("keto client construction is infallible"),
                ),
            ),
        ),
        Arc::new(sanvi_audit::adapters::AuditPrivacyProvider::new(
            tx.clone(),
            Arc::new(SqlxAuditReader::new()),
            deps.audit_writer.clone(),
        )),
        Arc::new(
            sanvi_platform_admin::adapters::privacy_provider::PlatformAdminPrivacyProvider::new(
                tx.clone(),
                Arc::new(sanvi_platform_admin::adapters::SqlxImpersonationGrantRepository::new()),
                Arc::new(sanvi_platform_admin::adapters::SqlxPendingApprovalRepository::new()),
                deps.privacy_lock.clone(),
            ),
        ),
        Arc::new(
            sanvi_billing::adapters::privacy_provider::BillingPrivacyProvider::new(
                tx.clone(),
                Arc::new(SqlxSubscriptionRepository::new()),
                Arc::new(SqlxCheckoutStateRepository::new()),
                Arc::new(SqlxInvoiceRepository::new()),
                Arc::new(SqlxWebhookEventRepository::new()),
                Arc::new(HttpStripeClient::new(
                    deps.stripe_api_base.clone(),
                    deps.stripe_api_key.expose_secret().clone(),
                    deps.stripe_api_version.clone(),
                    None,
                )),
            ),
        ),
        Arc::new(PrivacyContextProvider::new(
            tx.clone(),
            Arc::new(SqlxSubjectRepository::new()),
            Arc::new(SqlxConsentRepository::new()),
            Arc::new(SqlxSignalRepository::new()),
            Arc::new(SqlxDirectiveRepository::new()),
        )),
        Arc::new(sanvi_localization::adapters::LocalizationPrivacyProvider::new(tx.clone())),
        Arc::new(
            sanvi_theming::adapters::privacy_provider::ThemingPrivacyProvider::new(tx.clone()),
        ),
        Arc::new(
            sanvi_domains::adapters::privacy_provider::RemoteDomainsPrivacyProvider::new(
                &deps.domains_service,
            )
            .expect("validated Domains service configuration"),
        ),
        Arc::new(
            sanvi_payments::adapters::RemotePaymentsPrivacyProvider::new(&deps.payments_service)
                .expect("validated Payments service configuration"),
        ),
        Arc::new(
            sanvi_advertising::adapters::RemoteAdvertisingPrivacyProvider::new(
                &deps.advertising_service,
            )
            .expect("validated Advertising service configuration"),
        ),
        Arc::new(sanvi_media::adapters::MediaPrivacyProvider::with_storage(
            tx.clone(),
            deps.media.clone(),
        )),
        Arc::new(sanvi_notifications::NotificationsPrivacyProvider::new()),
    ]
}

/// Build the privacy context's application state. `providers` is the
/// registry built from every context crate's provider (missing one fails
/// the CI registry test); the audit adapter records every privacy action in
/// the phase-03 audit chain.
#[allow(clippy::too_many_arguments)]
pub fn build_privacy_state(
    pool: sanvi_persistence::PgPool,
    ids: std::sync::Arc<dyn sanvi_kernel::RawIdGenerator>,
    config: &PrivacyConfig,
    providers: ProviderRegistry,
    audit_writer: Arc<dyn sanvi_audit::domain::AuditWriter>,
    notification_localizer: Option<Arc<NotificationLocalizer>>,
    notification_gateway: Option<Arc<dyn NotificationGatewayPort>>,
) -> PrivacyWiring {
    let registered: std::collections::BTreeSet<_> = providers
        .iter()
        .map(|provider| provider.context_name())
        .collect();
    let expected: std::collections::BTreeSet<_> = sanvi_privacy::domain::CONTEXT_NAMES
        .iter()
        .copied()
        .collect();
    assert_eq!(
        registered, expected,
        "privacy provider registry must contain every context exactly once"
    );
    assert_eq!(
        providers.len(),
        sanvi_privacy::domain::CONTEXT_NAMES.len(),
        "privacy provider registry contains duplicate contexts"
    );
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
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

    let events: Arc<dyn PrivacyEventPublisher> = Arc::new(PrivacyEvents::new(ids.clone()));
    let notifications: Arc<dyn PrivacyNotifications> = match notification_localizer {
        Some(localizer) => Arc::new(LocalizedPrivacyNotifications {
            localizer,
            gateway: notification_gateway,
        }),
        None => Arc::new(PrivacyLoggingNotifications),
    };
    let clock: Arc<dyn PrivacyClock> = Arc::new(PrivacySystemClock);
    let ids: Arc<dyn PrivacyIds> = Arc::new(PrivacySystemIdGenerator::new(ids));
    let hasher: Arc<dyn Hasher> = Arc::new(Sha256Hasher::new(if config.kek.is_empty() {
        "dev-pepper-insecure".to_string()
    } else {
        config.kek.expose_secret().clone()
    }));
    let crypto: Arc<dyn EnvelopeCrypto> = if config.encryption_enabled() {
        let kek: [u8; 32] = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            config.kek.expose_secret(),
        )
        .expect("validated by PrivacyConfig")
        .try_into()
        .expect("validated length");
        Arc::new(
            SqlxEnvelopeCrypto::new(tx_manager.clone(), keys.clone(), kek, config.kek_id.clone())
                .with_erasure_ledger(config.erasure_ledger_dir.clone()),
        )
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
        events.clone(),
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
        events.clone(),
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
        signals.clone(),
        directives.clone(),
        events.clone(),
        ids.clone(),
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
        ids.clone(),
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
    let notices = Arc::new(
        NoticeService::new(
            tx_manager.clone(),
            rules.clone(),
            processors.clone(),
            jurisdictions.clone(),
        )
        .with_disclosures(config.disclosures.clone()),
    );

    // Worker jobs (the worker builds its own copies from the same config).
    let process_dsr = Arc::new(ProcessDsrBatch::new(
        tx_manager.clone(),
        requests.clone(),
        consents.clone(),
        participants.clone(),
        holds.clone(),
        artifacts.clone(),
        providers.clone(),
        packager,
        crypto.clone(),
        notifications.clone(),
        events.clone(),
        hasher.clone(),
        ids.clone(),
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
        subjects.clone(),
        clock.clone(),
        ids.clone(),
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
    ids: std::sync::Arc<dyn sanvi_kernel::RawIdGenerator>,
    config: &BillingConfig,
    entitlements_repo: Arc<dyn sanvi_access::domain::EntitlementRepository>,
    features_repo: Arc<dyn sanvi_access::domain::FeatureRepository>,
    audit_writer: Arc<dyn AuditWriter>,
    registry: &prometheus::Registry,
    degradation: std::sync::Arc<sanvi_resilience::DegradationRegistry>,
) -> sanvi_billing::adapters::http::BillingState {
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());

    let plans: Arc<dyn PlanRepository> = Arc::new(SqlxPlanRepository::new());
    let prices: Arc<dyn PlanPriceRepository> = Arc::new(SqlxPlanPriceRepository::new());
    let grants: Arc<dyn PlanEntitlementRepository> = Arc::new(SqlxPlanEntitlementRepository::new());
    let subscriptions: Arc<dyn SubscriptionRepository> =
        Arc::new(SqlxSubscriptionRepository::new());
    let checkout_states: Arc<dyn CheckoutStateRepository> =
        Arc::new(SqlxCheckoutStateRepository::new());
    let invoices: Arc<dyn InvoiceRepository> = Arc::new(SqlxInvoiceRepository::new());
    let webhook_events: Arc<dyn WebhookEventRepository> =
        Arc::new(SqlxWebhookEventRepository::new());
    let raw_ids = ids.clone();
    let ids: Arc<dyn BillingIds> = Arc::new(BillingSystemIdGenerator::new(ids));
    let clock: Arc<dyn BillingClock> = Arc::new(BillingSystemClock);

    let stripe_metrics = StripeMetrics::new(registry);
    let stripe: Arc<dyn StripeClient> = Arc::new(HttpStripeClient::new_with_degradation(
        config.stripe_api_base.clone(),
        config.stripe_api_key.expose_secret().clone(),
        config.stripe_api_version.clone(),
        Some(stripe_metrics),
        degradation,
    ));
    let tax_gate: Arc<dyn TaxGate> = Arc::new(CachedTaxGate::new(
        stripe.clone(),
        std::time::Duration::from_secs(300),
    ));

    let syncer: Arc<dyn EntitlementSyncer> =
        Arc::new(AccessEntitlementSyncer::new(entitlements_repo, raw_ids));

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
            checkout_states,
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
            config.stripe_webhook_secret.expose_secret().clone(),
            config.webhook_tolerance_secs,
            config.webhook_allowed_clients.clone(),
        )),
    }
}

/// The localization context's wiring: the negotiation middleware resolver,
/// the shared service (merged catalogs + cache + metrics), the synchronous
/// translator, the problem-body translator and the HTTP state.
#[derive(Clone)]
pub struct LocalizationWiring {
    pub state: sanvi_localization::adapters::http::LocalizationState,
    pub resolver: Arc<dyn sanvi_http::LocaleResolver>,
    pub translator: Arc<dyn LocalizationTranslator>,
    pub problem_translator: Arc<dyn sanvi_http::ProblemTranslator>,
    pub service: Arc<LocalizationService>,
    pub cache: Arc<LocalizationCache>,
}

/// Build the localization context's application state. The `LocaleResolver`
/// consumes the tenant settings repository, the supported-locale table and
/// the identity users table (via [`PoolUserLocaleProvider`]) — all cached —
/// so negotiation never hits the database on the hot path.
pub fn build_localization_state(
    pool: sanvi_persistence::PgPool,
    ids: std::sync::Arc<dyn sanvi_kernel::RawIdGenerator>,
    config: &LocalizationConfig,
    registry: &prometheus::Registry,
) -> LocalizationWiring {
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let tenant_locales: Arc<dyn TenantLocaleRepository> =
        Arc::new(SqlxTenantLocaleRepository::new());
    let supported: Arc<dyn SupportedLocaleRepository> =
        Arc::new(SqlxSupportedLocaleRepository::new());
    let translations: Arc<dyn TranslationRepository> = Arc::new(SqlxTranslationRepository::new());
    let events: Arc<dyn LocalizationEventPublisher> =
        Arc::new(LocalizationEvents::new(ids.clone()));
    let clock: Arc<dyn LocalizationClockTrait> = Arc::new(LocalizationClock);
    let users = Arc::new(PoolUserLocaleProvider::new(pool.clone()));

    let cache = Arc::new(LocalizationCache::new());
    let metrics = Arc::new(LocalizationMetrics::new(registry));
    let service = Arc::new(LocalizationService::new(
        pool.clone(),
        ids.clone(),
        translations.clone(),
        cache.clone(),
        metrics.clone(),
        config.clone(),
    ));

    let resolver: Arc<dyn sanvi_http::LocaleResolver> = Arc::new(LocaleResolverService::new(
        tx_manager,
        tenant_locales.clone(),
        supported.clone(),
        users,
        cache.clone(),
        metrics,
        service.clone(),
        config.clone(),
    ));
    let translator: Arc<dyn LocalizationTranslator> = service.clone();
    let problem_translator: Arc<dyn sanvi_http::ProblemTranslator> = service.clone();

    let catalog = Arc::new(CatalogService::new(
        pool.clone(),
        service.clone(),
        supported.clone(),
        config.default_locale.clone(),
    ));

    let state = sanvi_localization::adapters::http::LocalizationState {
        resolver: resolver.clone(),
        service: service.clone(),
        translator: translator.clone(),
        problem_translator: problem_translator.clone(),
        catalog: catalog.clone(),
        supported: supported.clone(),
        pool: pool.clone(),
        ids: ids.clone(),
        get_settings: Arc::new(GetLocalizationSettings::new(tenant_locales.clone())),
        update_settings: Arc::new(UpdateLocalizationSettings::new(
            tenant_locales,
            supported.clone(),
            pool.clone(),
            events.clone(),
            clock.clone(),
        )),
        get_overrides: Arc::new(GetTenantOverrides::new(translations.clone())),
        update_overrides: Arc::new(UpdateTenantOverrides::new(
            translations.clone(),
            events.clone(),
            clock.clone(),
        )),
        get_platform_translations: Arc::new(GetPlatformTranslations::new(
            translations.clone(),
            pool.clone(),
        )),
        update_platform_translations: Arc::new(UpdatePlatformTranslations::new(
            translations,
            events,
            clock,
        )),
    };

    LocalizationWiring {
        state,
        resolver,
        translator,
        problem_translator,
        service,
        cache,
    }
}

/// The theming context's wiring: the shared resolution service, the HTTP
/// state and the resolved-theme cache the worker's event handler also
/// consumes.
#[derive(Clone)]
pub struct ThemingWiring {
    pub state: sanvi_theming::adapters::http::ThemingState,
    pub service: Arc<ResolveThemeService>,
    pub cache: Arc<ResolvedThemeCache>,
}

/// The SSR cache's key resolver (TASK-022, FR-1111): host -> tenant via the
/// tenancy resolution order (signed header, custom domain, subdomain), then
/// the live theme revision through the theming resolution service — both
/// cache-first, so the hot path costs two Redis-shaped reads. A host that
/// resolves to no tenant (the marketing site, a garbage Host) yields `None`
/// and the request bypasses the cache.
struct StorefrontSsrKeyResolver {
    host_resolver: Arc<dyn sanvi_http::tenancy::TenantResolver>,
    themes: Arc<ResolveThemeService>,
}

impl sanvi_http::ssr_cache::SsrKeyResolver for StorefrontSsrKeyResolver {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        locale: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Option<sanvi_http::ssr_cache::SsrKeyResolution>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            match self.host_resolver.resolve(host, None).await {
                Ok(sanvi_http::tenancy::Resolution::Found(context)) => {
                    let resolved = self
                        .themes
                        .resolve_live(context.tenant_id, locale)
                        .await
                        .ok()?;
                    Some(sanvi_http::ssr_cache::SsrKeyResolution {
                        tenant_id: context.tenant_id,
                        theme_revision: resolved.revision,
                    })
                }
                _ => None,
            }
        })
    }
}

/// The theming context's feature-gate adapter over the access context's
/// domain-level `FeatureGate` (premium themes, custom CSS, custom
/// templates). The theming context never depends on the access crate.
pub struct ThemingAccessGate {
    gate: Arc<dyn FeatureGate>,
}

impl ThemingAccessGate {
    pub fn new(gate: Arc<dyn FeatureGate>) -> Self {
        Self { gate }
    }
}

#[async_trait::async_trait]
impl ThemeFeatureGate for ThemingAccessGate {
    async fn is_enabled(
        &self,
        tenant_id: sanvi_kernel::TenantId,
        feature: &str,
    ) -> Result<bool, sanvi_theming::domain::ThemingError> {
        let key = sanvi_access::domain::FeatureKey::parse(feature).map_err(|err| {
            sanvi_theming::domain::ThemingError::Storage(format!(
                "feature key {feature:?} is invalid: {err}"
            ))
        })?;
        self.gate
            .is_enabled(tenant_id, &key)
            .await
            .map_err(|err| sanvi_theming::domain::ThemingError::Storage(err.to_string()))
    }
}

/// Build the theming context's application state: repositories, the
/// entitlement gate, asset/artifact storage, the resolved-theme cache
/// (Redis, or a local fallback when Redis is disabled) and the resolution service.
pub fn build_theming_state(
    pool: sanvi_persistence::PgPool,
    ids: Arc<dyn sanvi_kernel::RawIdGenerator>,
    config: &ThemingConfig,
    registry: &prometheus::Registry,
    redis: Option<redis::aio::ConnectionManager>,
    feature_gate: Arc<dyn FeatureGate>,
    tenant_resolver: Arc<dyn sanvi_http::TenantResolver>,
) -> ThemingWiring {
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let themes: Arc<dyn ThemeRepository> = Arc::new(SqlxThemeRepository::new());
    let tenants: Arc<dyn TenantThemeRepository> = Arc::new(SqlxTenantThemeRepository::new());
    let gate: Arc<dyn ThemeFeatureGate> = Arc::new(ThemingAccessGate::new(feature_gate));
    let events: Arc<dyn ThemingEventPublisher> = Arc::new(ThemingEvents::new(ids.clone()));
    let clock: Arc<dyn ThemingClockTrait> = Arc::new(ThemingClock);
    let file_store = Arc::new(LocalFileStore::new(config.artifact_dir.clone()));
    let store: Arc<dyn BrandAssetStore> = file_store.clone();
    let artifact_store: Arc<dyn sanvi_theming::ArtifactStore> = file_store;
    let cache = Arc::new(ResolvedThemeCache::new(redis));
    let metrics = Arc::new(ThemingMetrics::new(registry));
    let host_resolver: Arc<dyn TenantContextResolver> =
        Arc::new(HostTenantResolverAdapter::new(tenant_resolver));

    let service = Arc::new(ResolveThemeService::new(
        themes.clone(),
        tenants.clone(),
        host_resolver,
        cache.clone(),
        metrics.clone(),
        clock.clone(),
        pool.clone(),
        ids.clone(),
        config.asset_base_url.clone(),
        config.resolved_cache_ttl_secs,
    ));

    let state = sanvi_theming::adapters::http::ThemingState {
        pool: pool.clone(),
        tx_manager,
        config: config.clone(),
        service: service.clone(),
        get_public_theme: Arc::new(ResolvePublicTheme::new(service.clone())),
        preview_theme: Arc::new(PreviewTenantTheme::new(service.clone())),
        get_draft: Arc::new(GetTenantThemeDraft::new(
            themes.clone(),
            tenants.clone(),
            pool.clone(),
            clock.clone(),
        )),
        update_draft: Arc::new(UpdateTenantThemeDraft::new(
            themes.clone(),
            tenants.clone(),
            gate.clone(),
            events.clone(),
            clock.clone(),
            pool.clone(),
            config.custom_css_max_bytes,
        )),
        publish: Arc::new(PublishTenantTheme::new(
            themes.clone(),
            tenants.clone(),
            gate.clone(),
            events.clone(),
            clock.clone(),
            pool.clone(),
            config.custom_css_max_bytes,
            cache.clone(),
        )),
        rollback: Arc::new(RollbackTenantTheme::new(
            tenants.clone(),
            events.clone(),
            clock.clone(),
            cache.clone(),
        )),
        available: Arc::new(ListAvailableThemes::new(
            themes.clone(),
            gate.clone(),
            pool.clone(),
            config.asset_base_url.clone(),
        )),
        upload_asset: Arc::new(UploadBrandAsset::new(
            tenants.clone(),
            themes.clone(),
            store.clone(),
            events.clone(),
            clock.clone(),
            pool.clone(),
            config.asset_max_bytes,
        )),
        remove_asset: Arc::new(RemoveBrandAsset::new(
            tenants.clone(),
            events.clone(),
            clock.clone(),
        )),
        install_version: Arc::new(InstallThemeVersion::new(
            themes.clone(),
            events.clone(),
            clock.clone(),
            pool.clone(),
            artifact_store,
            ids.clone(),
        )),
        yank_version: Arc::new(sanvi_theming::application::install::YankThemeVersion::new(
            themes.clone(),
            events,
            clock,
            pool.clone(),
        )),
        list_themes: Arc::new(ListThemesAdmin::new(themes.clone(), pool.clone())),
        list_versions: Arc::new(ListThemeVersionsAdmin::new(themes, pool)),
    };

    ThemingWiring {
        state,
        service,
        cache,
    }
}

// ---------------------------------------------------------------------------
// Phase 08: the domains context's composition root.
// ---------------------------------------------------------------------------

/// What the domains context needs from the composition root.
#[derive(Clone)]
pub struct DomainsWiring {
    pub state: sanvi_domains::adapters::http::DomainsState,
    pub lookup: Arc<LiveDomainLookupService>,
}

/// The domains context's feature-gate adapter over the access context's
/// domain-level `FeatureGate` (`domains.custom`, `domains.purchase`). The
/// domains context never depends on the access crate.
pub struct DomainsAccessGate {
    gate: Arc<dyn FeatureGate>,
}

impl DomainsAccessGate {
    pub fn new(gate: Arc<dyn FeatureGate>) -> Self {
        Self { gate }
    }
}

#[async_trait::async_trait]
impl DomainFeatureGate for DomainsAccessGate {
    async fn is_enabled(&self, tenant_id: TenantId, feature: &str) -> Result<bool, DomainsError> {
        let key = sanvi_access::domain::FeatureKey::parse(feature).map_err(|err| {
            DomainsError::Storage(format!("invalid feature key {feature:?}: {err}"))
        })?;
        self.gate
            .is_enabled(tenant_id, &key)
            .await
            .map_err(|err| DomainsError::Storage(err.to_string()))
    }
}

/// The domains context's guide translator over the localization context's
/// `Translator` (scope "platform"). With localization off, a fallback
/// returns the key itself.
pub struct DomainsGuideTranslatorAdapter {
    translator: Arc<dyn sanvi_localization::domain::Translator>,
}

impl DomainsGuideTranslatorAdapter {
    pub fn new(translator: Arc<dyn sanvi_localization::domain::Translator>) -> Self {
        Self { translator }
    }
}

impl GuideTranslator for DomainsGuideTranslatorAdapter {
    fn translate(&self, locale: &str, key: &str, args: &[(&str, String)]) -> String {
        let args: Vec<(&str, sanvi_localization::domain::Arg)> = args
            .iter()
            .map(|(name, value)| (*name, sanvi_localization::domain::Arg::Text(value.clone())))
            .collect();
        self.translator.translate("platform", locale, key, &args)
    }
}

/// The no-localization fallback: raw keys (the frontend renders its own
/// copy from the same contract).
pub struct FallbackGuideTranslator;

impl GuideTranslator for FallbackGuideTranslator {
    fn translate(&self, _locale: &str, key: &str, _args: &[(&str, String)]) -> String {
        key.to_string()
    }
}

/// The identity context's `TenantCatalog` port over the tenancy context's
/// `TenantRepository` — lets `GET /api/v1/me` resolve each membership's
/// tenant slug/display name without `sanvi-identity` depending on
/// `sanvi-tenancy` directly (mirrors `sanvi_access::adapters::SqlxRoleCatalog`,
/// the same shape one layer over).
pub struct TenantCatalogAdapter {
    tx_manager: sanvi_persistence::TxManager,
    tenants: Arc<dyn TenantRepository>,
}

impl TenantCatalogAdapter {
    pub fn new(
        tx_manager: sanvi_persistence::TxManager,
        tenants: Arc<dyn TenantRepository>,
    ) -> Self {
        Self {
            tx_manager,
            tenants,
        }
    }
}

#[async_trait::async_trait]
impl TenantCatalog for TenantCatalogAdapter {
    async fn summaries(
        &self,
        tenant_ids: &[TenantId],
    ) -> Result<Vec<IdentityTenantSummary>, IdentityError> {
        if tenant_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut scope = self
            .tx_manager
            .begin_unscoped("tenant_catalog", "identity")
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        let mut summaries = Vec::with_capacity(tenant_ids.len());
        for &tenant_id in tenant_ids {
            let tenant = self
                .tenants
                .find_by_id(&mut scope, tenant_id)
                .await
                .map_err(|err| IdentityError::Storage(err.to_string()))?;
            // A membership referencing a since-deleted tenant is skipped
            // rather than surfaced with blank metadata (see get_me.rs).
            if let Some(tenant) = tenant {
                summaries.push(IdentityTenantSummary {
                    tenant_id: tenant.id(),
                    slug: tenant.slug().as_str().to_string(),
                    name: tenant.display_name().as_str().to_string(),
                });
            }
        }
        scope
            .commit()
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        Ok(summaries)
    }
}

/// The domains context's tenant-state port over the tenancy context's
/// `ResolveTenant`.
pub struct DomainsTenantStateAdapter {
    resolve: Arc<sanvi_tenancy::application::ResolveTenant>,
}

impl DomainsTenantStateAdapter {
    pub fn new(resolve: Arc<sanvi_tenancy::application::ResolveTenant>) -> Self {
        Self { resolve }
    }
}

#[async_trait::async_trait]
impl TenantState for DomainsTenantStateAdapter {
    async fn is_active(&self, tenant_id: TenantId) -> Result<bool, DomainsError> {
        let tenant = self
            .resolve
            .by_id(tenant_id)
            .await
            .map_err(|err| DomainsError::Storage(err.to_string()))?;
        Ok(matches!(
            tenant.map(|tenant| tenant.status()),
            Some(sanvi_tenancy::domain::TenantStatus::Active)
        ))
    }
}

/// The phase-08 hook for the tenancy resolver: wraps the domains lookup
/// service in the trait `HostTenantResolver` consumes.
pub struct DomainsTenantLookupAdapter {
    lookup: Arc<LiveDomainLookupService>,
}

impl DomainsTenantLookupAdapter {
    pub fn new(lookup: Arc<LiveDomainLookupService>) -> Self {
        Self { lookup }
    }
}

#[async_trait::async_trait]
impl sanvi_tenancy::adapters::resolution::CustomDomainLookup for DomainsTenantLookupAdapter {
    async fn lookup(
        &self,
        host: &str,
    ) -> Result<Option<TenantId>, sanvi_tenancy::domain::TenancyError> {
        Ok(self.lookup.resolve(host).await)
    }
}

/// Build the host → tenant lookup service (needed before the tenancy
/// resolver, so it exists as its own step in `main`).
pub fn build_domains_lookup(
    pool: sanvi_persistence::PgPool,
    redis: Option<redis::aio::ConnectionManager>,
    config: &DomainsConfig,
) -> Arc<LiveDomainLookupService> {
    let cache = Arc::new(LiveDomainCache::new(redis, config.host_cache_ttl_secs));
    let domains: Arc<dyn CustomDomainRepository> = Arc::new(SqlxCustomDomainRepository::new());
    let source = Arc::new(sanvi_domains::adapters::RepositoryHostnameSource::new(
        domains, pool,
    ));
    Arc::new(LiveDomainLookupService::new(cache, source))
}

/// Build the remote host → tenant lookup service backed by the standalone Domains service.
pub fn build_remote_domains_lookup(
    redis: Option<redis::aio::ConnectionManager>,
    config: &DomainsConfig,
    service_config: &sanvi_domains::adapters::remote::ServiceConfig,
) -> anyhow::Result<Arc<LiveDomainLookupService>> {
    let cache = Arc::new(LiveDomainCache::new(redis, config.host_cache_ttl_secs));
    let client = sanvi_domains::adapters::remote::RemoteClient::new(service_config)?;
    let source = Arc::new(sanvi_domains::adapters::RemoteHostnameSource::new(client));
    Ok(Arc::new(LiveDomainLookupService::new(cache, source)))
}

/// Phase 10's registry-derived capability catalog. When `real_platform_adapters`
/// is false (default), the in-process adapter is used for every platform; when
/// true, real Google Ads / Meta adapters are registered where configured,
/// falling back to the fake adapter per platform if credentials are missing
/// or invalid.
///
/// `directive_resolver` and `subprocessors` come from the privacy context
/// (phase 05): the directive gate (TASK-014) is the phase's core
/// invariant, and advertising holds no directive or sub-processor state of
/// its own to consult instead.
///
/// Tenant timezones likewise live in the localization context: budget periods
/// are calendar periods in the tenant timezone (TASK-017), and advertising
/// must not import another context's domain, so this adapter implements the
/// advertising port from the localization repository here at the composition
/// Build the media context's application state: the `hitox-media-service`
/// HTTP gateway wrapped in the six asset use cases. Unlike the DB-backed
/// contexts this is sync and infallible (an unconfigured service fails
/// closed per call with `MediaError::Unavailable`), so `build_app` calls it
/// directly instead of `main.rs`.
pub fn build_media_state(
    config: &sanvi_media::MediaConfig,
    degradation: std::sync::Arc<sanvi_resilience::DegradationRegistry>,
    tx: sanvi_persistence::TxManager,
) -> sanvi_media::adapters::http::MediaState {
    let gateway: std::sync::Arc<dyn sanvi_media::domain::MediaGatewayPort> = std::sync::Arc::new(
        sanvi_media::adapters::hitox::HitoxMediaAdapter::new_with_degradation(
            config.clone(),
            degradation,
        ),
    );
    sanvi_media::adapters::http::MediaState {
        create_asset_upload: std::sync::Arc::new(
            sanvi_media::application::CreateAssetUpload::new(gateway.clone()).with_owner_index(
                Arc::new(sanvi_media::adapters::SqlxMediaOwnerIndex::new(tx)),
            ),
        ),
        presign_asset_parts: std::sync::Arc::new(sanvi_media::application::PresignAssetParts::new(
            gateway.clone(),
        )),
        complete_asset_upload: std::sync::Arc::new(
            sanvi_media::application::CompleteAssetUpload::new(gateway.clone()),
        ),
        get_media_asset: std::sync::Arc::new(sanvi_media::application::GetMediaAsset::new(
            gateway.clone(),
        )),
        delete_media_asset: std::sync::Arc::new(sanvi_media::application::DeleteMediaAsset::new(
            gateway.clone(),
        )),
        create_asset_delivery: std::sync::Arc::new(
            sanvi_media::application::CreateAssetDelivery::new(gateway),
        ),
    }
}

/// Build the domains context's application state: repositories, the
/// entitlement gate, the sandbox registrar/DNS/CA gateways and every use
/// case. (The DoH verifier, prober and notification stub belong to the
/// worker's jobs and are built there.)
#[allow(clippy::too_many_arguments)]
pub fn build_domains_state(
    pool: sanvi_persistence::PgPool,
    ids: std::sync::Arc<dyn sanvi_kernel::RawIdGenerator>,
    config: &DomainsConfig,
    registry: &prometheus::Registry,
    redis: Option<redis::aio::ConnectionManager>,
    feature_gate: Arc<dyn FeatureGate>,
    translator: Arc<dyn GuideTranslator>,
    lookup: Arc<LiveDomainLookupService>,
    tenancy_resolve: Arc<sanvi_tenancy::application::ResolveTenant>,
) -> DomainsWiring {
    let tx = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());

    let domains: Arc<dyn CustomDomainRepository> = Arc::new(SqlxCustomDomainRepository::new());
    let orders: Arc<dyn DomainOrderRepository> = Arc::new(SqlxDomainOrderRepository::new());
    let certs: Arc<dyn sanvi_domains::domain::CertificateRepository> =
        Arc::new(SqlxCertificateRepository::new());
    let records: Arc<dyn sanvi_domains::domain::DnsRecordRepository> =
        Arc::new(SqlxDnsRecordRepository::new(ids.clone()));
    let gate: Arc<dyn DomainFeatureGate> = Arc::new(DomainsAccessGate::new(feature_gate));
    let events: Arc<dyn DomainsEventPublisher> = Arc::new(DomainsEvents::new(ids.clone()));
    let clock: Arc<dyn sanvi_domains::domain::Clock> = Arc::new(DomainsClock);
    let metrics = Arc::new(DomainsMetrics::new(registry));

    let (registrar, registrar_mode) =
        build_domain_registrar(config).expect("registrar adapter must initialize cleanly");
    let provider: Arc<dyn sanvi_domains::domain::DnsProviderPort> =
        Arc::new(SqlxDnsProvider::new(records.clone()));
    let (ca, ca_mode) = build_certificate_authority(config, ids.clone())
        .expect("certificate authority adapter must initialize cleanly");
    let (charger, charger_mode) =
        build_purchase_charger(config).expect("purchase charger adapter must initialize cleanly");
    tracing::info!(
        registrar_mode = %registrar_mode,
        ca_mode = %ca_mode,
        charger_mode = %charger_mode,
        "domains capabilities initialized"
    );
    let tenant_state: Arc<dyn TenantState> =
        Arc::new(DomainsTenantStateAdapter::new(tenancy_resolve));

    let search_cache = Arc::new(sanvi_domains::application::search::SearchCache::new());

    let place_order = Arc::new(PlaceDomainOrder::new(
        orders.clone(),
        domains.clone(),
        registrar.clone(),
        provider.clone(),
        charger.clone(),
        gate.clone(),
        events.clone(),
        clock.clone(),
        ids.clone(),
        tx.clone(),
        config.platform_domain.clone(),
        config.edge_hostname.clone(),
        config.anycast_ips.clone(),
        config.purchase_entitlement_key.clone(),
        config.verification_timeout_hours,
        config.verification_backoff_base_secs,
    ));

    let state = sanvi_domains::adapters::http::DomainsState {
        pool: pool.clone(),
        tx: tx.clone(),
        config: config.clone(),
        metrics: metrics.clone(),
        lookup: lookup.clone(),
        cache: lookup.cache(),
        claim: Arc::new(ClaimCustomDomain::new(
            domains.clone(),
            gate.clone(),
            events.clone(),
            clock.clone(),
            ids.clone(),
            tx.clone(),
            config.platform_domain.clone(),
            config.connect_entitlement_key.clone(),
            config.verification_timeout_hours,
            config.verification_backoff_base_secs,
        )),
        list_domains: Arc::new(ListCustomDomains::new(domains.clone(), certs.clone())),
        list_orders: Arc::new(ListDomainOrders::new(orders.clone())),
        instructions: Arc::new(GetDomainInstructions::new(
            domains.clone(),
            translator,
            config.edge_hostname.clone(),
            config.anycast_ips.clone(),
        )),
        verify: Arc::new(RequestVerification::new(domains.clone(), clock.clone())),
        remove: Arc::new(RemoveCustomDomain::new(
            domains.clone(),
            certs.clone(),
            provider.clone(),
            ca.clone(),
            events.clone(),
            clock.clone(),
        )),
        promote: Arc::new(PromotePrimaryDomain::new(
            domains.clone(),
            events.clone(),
            clock.clone(),
        )),
        search: Arc::new(SearchDomains::new(
            registrar.clone(),
            gate.clone(),
            search_cache,
            config.purchase_entitlement_key.clone(),
            config.registrar.search_cache_ttl_secs,
        )),
        place_order: place_order.clone(),
        set_auto_renew_on: Arc::new(SetOrderAutoRenew::new(orders.clone(), clock.clone(), true)),
        set_auto_renew_off: Arc::new(SetOrderAutoRenew::new(orders.clone(), clock.clone(), false)),
        authorize_tls: Arc::new(AuthorizeTls::new(
            domains.clone(),
            tenant_state,
            pool.clone(),
        )),
        platform_list: Arc::new(PlatformListDomains::new(domains.clone(), tx.clone())),
        platform_release: Arc::new(PlatformReleaseDomain::new(
            domains.clone(),
            certs,
            provider,
            ca,
            events.clone(),
            tx.clone(),
        )),
    };

    let _ = redis;

    DomainsWiring { state, lookup }
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
    localization: Option<LocalizationWiring>,
    theming: Option<ThemingWiring>,
    domains: Option<DomainsWiring>,
    advertising: Option<sanvi_advertising::adapters::remote::GatewayState>,
    accounting: Option<sanvi_accounting::adapters::http::AccountingState>,
    redis: Option<redis::aio::ConnectionManager>,
    ssr_cache: Option<std::sync::Arc<sanvi_cache::SsrCache>>,
    degradation: std::sync::Arc<sanvi_resilience::DegradationRegistry>,
    // Process-global shedding counters, built once by the composition
    // root (TASK-023, FR-1114). `None` admits everything; the flag inside
    // `server.load_shedding` still decides.
    load_shed: Option<std::sync::Arc<sanvi_http::LoadShedState>>,
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
    let _ = advertising;
    let advertising =
        config
            .features
            .advertising
            .then(|| sanvi_advertising::adapters::remote::GatewayState {
                remote: sanvi_advertising::adapters::remote::RemoteClient::new(
                    &config.advertising_service,
                )
                .expect("validated Advertising service configuration"),
                authorizer: Arc::new(advertising::AuthorizerAdvertisingAccess(
                    access.authorizer.clone(),
                )),
            });

    let state = AppState {
        build_info: system.build_info.execute(),
        metrics,
        config: config.clone(),
    };

    // Captured before `state` moves into app_data (TASK-022 11.3c).
    let ssr_cache_config = server_config.ssr_cache.clone();

    // Media ships ungated (no feature flag): the state is a plain
    // HTTP-client wrapper over `config.media`, built infallibly here.
    let media_state = build_media_state(
        &config.media,
        degradation.clone(),
        access.tx_manager.clone(),
    );
    let product_access_state =
        product_access::build_product_access_state(&config.product_access, &access);

    // The billing context mounts its tenant routes inside the shared tenant
    // scope (a second scope on the same prefix would shadow the first).
    let tenant_scope = web::scope("/api/v1/tenant")
        .configure(tenancy_routes::tenant_scope_routes)
        .configure(identity_routes::tenant_scope_routes)
        .configure(sanvi_access::routes::tenant_scope_routes);
    let tenant_scope = match &billing {
        Some(_) => tenant_scope.configure(billing_routes::tenant_scope_routes),
        None => tenant_scope,
    };
    let tenant_scope = match &privacy {
        Some(_) => tenant_scope.configure(sanvi_privacy::routes::tenant_scope_routes),
        None => tenant_scope,
    };
    let tenant_scope = match &localization {
        Some(_) => tenant_scope.configure(localization_routes::localization_tenant_scope_routes),
        None => tenant_scope,
    };
    let tenant_scope = match &theming {
        Some(theming) => {
            let asset_max_bytes = theming.state.config.asset_max_bytes;
            tenant_scope.configure(move |cfg| {
                theming_routes::theming_tenant_scope_routes_with_asset_limit(cfg, asset_max_bytes)
            })
        }
        None => tenant_scope,
    };
    let tenant_scope = if config.features.domains {
        tenant_scope.configure(domains_routes::domains_tenant_scope_routes)
    } else {
        tenant_scope
    };
    let tenant_scope = if config.features.payments {
        tenant_scope.configure(payments_routes::payments_tenant_scope_routes)
    } else {
        tenant_scope
    };
    let tenant_scope = if config.features.advertising {
        match &advertising {
            Some(_) => tenant_scope
                .configure(advertising_routes::advertising_tenant_scope_routes)
                .configure(advertising_routes::tracking_tenant_routes),
            None => tenant_scope,
        }
    } else {
        tenant_scope
    };
    let tenant_scope = if config.features.accounting {
        tenant_scope.configure(sanvi_accounting::adapters::http::accounting_tenant_scope_routes)
    } else {
        tenant_scope
    };
    // `media_tenant_scope_routes` registers `/v1/assets...` with no leading
    // context segment, so it needs the extra `web::scope("/media")` wrapper
    // here (contrast with advertising, whose routes carry `/ads/...`
    // themselves and are `.configure()`'d directly).
    let tenant_scope = tenant_scope
        .service(web::scope("/media").configure(media_routes::media_tenant_scope_routes))
        // `/ecommerce/{tail}` forwards to ecommerce-service; unconfigured,
        // every request answers 503 after the entitlement and permission
        // checks.
        .configure(sanvi_product_access::routes::product_access_tenant_scope_routes);

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
        .app_data(web::Data::new(media_state))
        .app_data(web::Data::new(product_access_state))
        .app_data(web::Data::new(sanvi_accounting::adapters::remote::GatewayState {
            remote: sanvi_accounting::adapters::remote::RemoteClient::new(&config.accounting_service)
                .expect("validated Accounting service configuration"),
            access: Arc::new(accounting::AuthorizerAccess(access.authorizer.clone())),
        }))
        .app_data(web::Data::new(accounting::AuditIngestState {
            secret: config.accounting_service.inbound_secret.clone(),
            transactions: access.tx_manager.clone(),
            writer: audit.writer.clone(),
        }))
        .configure(accounting::internal_routes)
        .app_data(web::Data::new(sanvi_domains::adapters::remote::GatewayState {
            remote: sanvi_domains::adapters::remote::RemoteClient::new(&config.domains_service)
                .expect("validated Domains service configuration"),
            authorizer: Arc::new(domains::AuthorizerDomainsAccess(access.authorizer.clone())),
            feature_gate: Arc::new(DomainsAccessGate::new(access.domain_feature_gate.clone())),
            connect_entitlement_key: config.domains.connect_entitlement_key.clone(),
            purchase_entitlement_key: config.domains.purchase_entitlement_key.clone(),
        }))
        .app_data(web::Data::new(domains::DomainsIngestState {
            secret: config.domains_service.inbound_secret.clone(),
            transactions: access.tx_manager.clone(),
            writer: audit.writer.clone(),
            resolve_tenant: tenancy.resolve.clone(),
        }))
        .configure(domains::internal_routes)
        .app_data(web::Data::new(sanvi_payments::adapters::remote::GatewayState {
            remote: sanvi_payments::adapters::remote::RemoteClient::new(&config.payments_service)
                .expect("validated Payments service configuration"),
            authorizer: Arc::new(payments::AuthorizerPaymentsAccess(access.authorizer.clone())),
        }))
        .app_data(web::Data::new(payments::PaymentsIngestState {
            secret: config.payments_service.inbound_secret.clone(),
            transactions: access.tx_manager.clone(),
            writer: audit.writer.clone(),
        }))
        .configure(payments::internal_routes)
        .app_data(web::Data::new(advertising::AdvertisingPlatformState {
            secret: config.advertising_service.inbound_secret.clone(),
            transactions: access.tx_manager.clone(),
            directives: privacy.as_ref().map(|w| w.state.directive_resolver.clone()),
            feature_gate: access.feature_gate.clone(),
            enabled: config.features.advertising,
        }))
        .configure(advertising::internal_routes)
        .app_data(web::JsonConfig::default().limit(server_config.max_body_bytes))
        .app_data(web::PayloadConfig::default().limit(server_config.max_body_bytes))
        .configure(health_routes)
        .configure(platform_routes)
        // Deeper platform_admin routes (`/platform/tenants/{id}/…`) must be
        // registered before tenancy's shorter `/platform/tenants/{id}` —
        // registering the shorter dynamic resource first silently shadows
        // the longer siblings in actix's route tree (they 404).
        .configure(platform_admin_routes::platform_admin_routes)
        .configure(tenancy_routes::platform_routes)
        .service(tenant_scope)
        .configure(tenancy_routes::public_routes)
        .configure(identity_routes::identity_routes)
        .configure(identity_routes::internal_routes)
        .configure(sanvi_access::routes::access_routes)
        .configure(sanvi_audit::routes::audit_routes)
        .service(web::scope("/api/v1/system").configure(api_routes));

    // Phase-04 gate: with the flag off, the billing state is not wired and
    // no billing route is mounted.
    let advertising_mounted = advertising.is_some();
    let app = match billing {
        Some(billing_state) => app
            .app_data(web::Data::new(billing_state))
            .configure(billing_routes::public_routes)
            .configure(billing_routes::billing_platform_routes)
            .configure(billing_routes::billing_internal_routes),
        None => app,
    };

    let app = match &tenancy.quotas {
        Some(service) => app
            .app_data(web::Data::new(sanvi_http::quota_middleware::QuotaRuntime {
                service: service.clone(),
                enabled: config.platform.quotas,
            }))
            .app_data(web::Data::new(Arc::new(
                sanvi_http::api_limits::ApiLimits::new(
                    redis.clone(),
                    Arc::new(QuotaPlanResolver(service.clone())),
                    config.platform.rate_limits,
                ),
            ))),
        None => app,
    };

    let app = match advertising {
        Some(state) => app
            .app_data(web::Data::new(state))
            .configure(advertising_routes::advertising_platform_routes),
        None => app,
    };

    let app = match accounting {
        Some(state) => app.app_data(web::Data::new(state)),
        None => app,
    };

    // Phase-05 gate: privacy routes (subject-scoped scope wrapped with the
    // GPC middleware, public disclosures, platform control plane). The
    // storefront track beacon is wrapped with the same middleware when the
    // privacy context is wired: `Sec-GPC` arrives on the tenant's own
    // domain, on the very first request — the storefront code path phase
    // 05's original scope never saw (TASK-029 11.10 proves it by drill).
    // With advertising off there is no beacon to wrap; with privacy off the
    // beacon mounts unwrapped, as before.
    let app = match privacy {
        Some(wiring) => {
            let gpc = wiring.gpc.clone();
            let app = app
                .app_data(web::Data::new(wiring.state))
                .configure(sanvi_privacy::routes::public_routes)
                .configure(sanvi_privacy::routes::privacy_platform_routes)
                .service(
                    web::scope("/api/v1/privacy")
                        .wrap(wiring.gpc)
                        .configure(sanvi_privacy::routes::privacy_routes),
                );
            if advertising_mounted {
                app.service(
                    web::scope("")
                        .wrap(gpc)
                        .configure(advertising_routes::tracking_public_routes),
                )
            } else {
                app
            }
        }
        None => match advertising_mounted {
            // The advertising state was registered above; the beacon mounts
            // unwrapped because no GPC middleware exists without privacy.
            true => app.configure(advertising_routes::tracking_public_routes),
            false => app,
        },
    };

    // Phase-06 gate: localization routes plus the problem-body translator
    // the error layer consumes. With the flag off, no locale negotiation
    // and English-only problem bodies.
    let app = match localization.clone() {
        Some(wiring) => app
            .app_data(web::Data::new(wiring.state))
            .app_data(web::Data::new(sanvi_http::error::ProblemTranslatorData(
                wiring.problem_translator.clone(),
            )))
            .configure(localization_routes::localization_public_routes)
            .configure(localization_routes::localization_platform_routes),
        None => app,
    };

    // Phase-07 gate: the theming context's public and platform routes (the
    // tenant routes mount inside the shared tenant scope above).
    let app = match theming.clone() {
        Some(wiring) => app
            .app_data(web::Data::new(wiring.state))
            .configure(theming_routes::theming_public_routes)
            .configure(theming_routes::theming_platform_routes),
        None => app,
    };

    // Phase-08 gate: the domains context's platform and internal routes
    // (the tenant routes mount inside the shared tenant scope above). The
    // `/internal/tls/authorize` gate is self-authenticated — `/internal`
    // sits outside the authn middleware by design.
    let app = match domains.clone() {
        Some(wiring) => app
            .app_data(web::Data::new(wiring.state))
            .configure(domains_routes::domains_platform_routes)
            .configure(domains_routes::domains_internal_routes),
        None => {
            if config.features.domains {
                app.configure(domains_routes::domains_platform_routes)
                    .configure(domains_routes::domains_internal_routes)
            } else {
                app
            }
        }
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
                vec![
                    "/healthz".to_string(),
                    "/readyz".to_string(),
                    "/metrics".to_string(),
                    "/openapi.json".to_string(),
                    "/scalar".to_string(),
                    "/api/v1/public".to_string(),
                    // FR-1102: the build probe is the incident-time stamp —
                    // it must not run session verification, or a
                    // same-origin caller's cookie turns "identity backend
                    // down" into a 503 here too (it is tenant-exempt for
                    // the same reason).
                    "/api/v1/system/build".to_string(),
                    "/internal".to_string(),
                ]
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

    // The phase-06 feature gate: with the flag off, no locale negotiation
    // and no localized problem bodies (every response stays English).
    let locale_middleware = if config.features.localization {
        localization.as_ref().map(|wiring| {
            Arc::new(sanvi_http::LocaleMiddleware {
                resolver: wiring.resolver.clone(),
            })
        })
    } else {
        None
    };

    // TASK-022 (FR-1111): the keyed SSR response cache over the anonymous
    // storefront reads. The cache itself (and its counters) is built once in
    // the binary's main and passed in — `build_app` runs once per actix
    // worker, and a per-worker registration would leave 19 of 20 workers'
    // counter increments invisible on /metrics. The shared Arc also makes
    // the single-flight map process-wide. The flag-off default (and any
    // build without the cache or the theming context) assembles the
    // pass-through layer: every public read renders, exactly the pre-cache
    // behaviour.
    let ssr_cache = match (config.server.ssr_cache.enabled, ssr_cache, theming.as_ref()) {
        (true, Some(cache), Some(theming)) => {
            let resolver: std::sync::Arc<dyn sanvi_http::ssr_cache::SsrKeyResolver> =
                std::sync::Arc::new(StorefrontSsrKeyResolver {
                    host_resolver: tenancy.host_resolver.clone(),
                    themes: theming.service.clone(),
                });
            Some(sanvi_http::ssr_cache::SsrCacheLayer::new(
                ssr_cache_config.clone(),
                (*cache).clone(),
                resolver,
            ))
        }
        _ => None,
    };
    let ssr_cache = ssr_cache
        .unwrap_or_else(|| sanvi_http::ssr_cache::SsrCacheLayer::disabled(ssr_cache_config));

    // TASK-023 (FR-1113): the degraded-mode header. The flag-off assembly
    // is a pass-through; the flag-on assembly stamps `X-Sanvi-Degraded` on
    // every response served while a dependency runs degraded.
    let degraded = if server_config.degraded_mode.enabled {
        sanvi_http::DegradedHeaderLayer::new(degradation)
    } else {
        sanvi_http::DegradedHeaderLayer::disabled()
    };

    middleware::stack(
        app,
        server_config,
        redis,
        tenant_middleware,
        authn,
        impersonation,
        locale_middleware,
        ssr_cache,
        degraded,
        load_shed,
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
                fields: None,
                reason: None,
                extensions: None,
            })
        }
    }
}

/// The merged contract document, rendered once: it is static per build, so
/// rebuilding and re-serializing it per request (~25 ms avg in the 11.2c
/// profile, the second-hottest endpoint) is pure waste. The validator is a
/// strong ETag over the rendered bytes, so conditional requests revalidate
/// without a body at all.
///
/// `Cache-Control: public, no-cache` lets shared caches store the document
/// but forces revalidation on every reuse, so a deploy is visible as soon as
/// the ETag moves. (The platform ETag middleware leaves this response alone
/// — it already carries a validator.)
static OPENAPI_JSON: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();

fn rendered_openapi() -> &'static (String, String) {
    OPENAPI_JSON.get_or_init(|| {
        use utoipa::OpenApi as _;
        let body = ApiDoc::openapi()
            .to_json()
            .expect("openapi serialization is infallible");
        let etag = sanvi_http::etag_for_bytes(body.as_bytes());
        (body, etag)
    })
}

/// `/openapi.json` handler: the pre-rendered contract document.
async fn openapi_json(req: HttpRequest) -> HttpResponse {
    let (body, etag) = rendered_openapi();
    if sanvi_http::if_none_match_matches(req.headers().get(header::IF_NONE_MATCH), etag) {
        return HttpResponse::NotModified()
            .insert_header(("etag", etag.clone()))
            .insert_header(("cache-control", "public, no-cache"))
            .finish();
    }
    HttpResponse::Ok()
        .insert_header(("etag", etag.clone()))
        .insert_header(("cache-control", "public, no-cache"))
        .content_type("application/json")
        .body(body.clone())
}
