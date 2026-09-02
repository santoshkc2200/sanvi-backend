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
use sanvi_advertising::{
    AdvertisingConfig, OAuthStateManager,
    adapters::{
        FakeAdPlatform, SqlxAdvertisingConnectionRepository, SqlxCampaignRepository,
        SqlxConnectionTokenRepository, SqlxCreativeRepository, SqlxOAuthStateStore,
        SqlxTenantEnvelopeCrypto,
    },
    application::{
        AddAd, AddAdGroup, CreateCampaign, CreateConnection, CreateCreative, DeleteConnection,
        DeleteCreative, GetCampaign, GetCreative, GetCreativePreviews, GetFreshness,
        HandleCallback, ListCampaignChanges, ListCampaigns, ListConnections, ListCreatives,
        ListPlatformCatalog, PatchCampaign, PublishCampaign, QueryMetrics, ReconcileCampaign,
        ResolveCampaignDrift, ResumeCampaign, StartAuthorization, UpdateAd, UpdateAdGroup,
        ValidateCampaign,
    },
    domain::TokenVault,
    routes as advertising_routes,
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
        SandboxCertificateAuthority, SandboxRegistrar, SqlxCertificateRepository,
        SqlxCustomDomainRepository, SqlxDnsProvider, SqlxDnsRecordRepository,
        SqlxDomainOrderRepository, SystemClock as DomainsClock,
    },
    application::{
        AuthorizeTls, ClaimCustomDomain, GetDomainInstructions, ListCustomDomains,
        ListDomainOrders, LiveDomainLookupService, PlaceDomainOrder, PlatformListDomains,
        PlatformReleaseDomain, PromotePrimaryDomain, RemoveCustomDomain, RequestVerification,
        SearchDomains, SetOrderAutoRenew,
    },
    domain::{
        CustomDomainRepository, DomainFeatureGate, DomainOrderRepository, DomainsError,
        EventPublisher as DomainsEventPublisher, GuideTranslator, PurchaseCharger, TenantState,
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
use sanvi_payments::{
    PaymentsConfig,
    adapters::{
        FakeProvider, FixedWindowThrottle, OutboxEventPublisher as PaymentsEvents, PaymentsMetrics,
        SqlxCheckoutRepository, SqlxPayoutRepository, SqlxProviderConnectionRepository,
        SqlxTaxSettingsRepository,
    },
    application::{
        ConfiguredApplicationFeePolicy, CreateCheckout, ListPayouts, ListProviders,
        ManageTaxSettings, ProviderRegistry as PaymentsProviderRegistry, RegistryProviderCatalog,
        RuntimeEnvironment, StartProviderConnection,
    },
    routes as payments_routes,
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

struct LocalizedIdentityNotifications {
    localizer: Arc<NotificationLocalizer>,
}

#[async_trait::async_trait]
impl Notifications for LocalizedIdentityNotifications {
    async fn send_invitation(
        &self,
        tenant_id: TenantId,
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
        tracing::info!(
            to = %email,
            tenant_id = %tenant_id,
            locale = %context.locale,
            subject,
            body,
            notification = "invitation",
            "localized notification rendered"
        );
        Ok(())
    }

    async fn send_security_notice(
        &self,
        email: &sanvi_identity::domain::Email,
        _kind: &str,
        detail: &str,
    ) -> Result<(), IdentityError> {
        let context = self.localizer.platform_locale().await;
        let subject = self.localizer.render(
            &context,
            "emails.security_notice.subject",
            &[("tenant", LocalizationArg::from("Sanvi"))],
        );
        let body = self.localizer.render(
            &context,
            "emails.security_notice.body",
            &[("detail", LocalizationArg::from(detail))],
        );
        tracing::info!(
            to = %email,
            locale = %context.locale,
            subject,
            body,
            notification = "security_notice",
            "localized notification rendered"
        );
        Ok(())
    }
}

/// External repository, catalog, and localizer dependencies for identity state.
pub struct IdentityDependencies {
    pub role_catalog: Arc<dyn RoleCatalog>,
    pub tenant_catalog: Arc<dyn TenantCatalog>,
    pub notification_localizer: Option<Arc<NotificationLocalizer>>,
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
        Some(localizer) => Arc::new(LocalizedIdentityNotifications { localizer }),
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
) -> AccessWiring {
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
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
    let events: Arc<dyn AccessEventPublisher> = Arc::new(OutboxEventPublisher::new(ids.clone()));
    let clock: Arc<dyn sanvi_access::domain::Clock> = Arc::new(sanvi_access::adapters::SystemClock);
    let ids: Arc<dyn sanvi_access::domain::IdGenerator> =
        Arc::new(sanvi_access::adapters::SystemRoleIdGenerator::new(ids));

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
pub fn build_audit_state(
    pool: sanvi_persistence::PgPool,
    ids: std::sync::Arc<dyn sanvi_kernel::RawIdGenerator>,
    config: &AuditConfig,
) -> AuditWiring {
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let writer: Arc<dyn AuditWriter> = Arc::new(SqlxAuditWriter::new(ids.clone()));
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

struct LocalizedPrivacyNotifications {
    localizer: Arc<NotificationLocalizer>,
}

#[async_trait::async_trait]
impl PrivacyNotifications for LocalizedPrivacyNotifications {
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
        tracing::info!(
            subject_ref = %subject_ref.canonical_key(),
            token,
            locale = %context.locale,
            subject,
            body,
            "localized privacy export notification rendered"
        );
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
        Some(localizer) => Arc::new(LocalizedPrivacyNotifications { localizer }),
        None => Arc::new(PrivacyLoggingNotifications),
    };
    let clock: Arc<dyn PrivacyClock> = Arc::new(PrivacySystemClock);
    let ids: Arc<dyn PrivacyIds> = Arc::new(PrivacySystemIdGenerator::new(ids));
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
            config.stripe_webhook_secret.clone(),
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

/// The reference purchase charger: deterministic sandbox charges recorded
/// in the audit log. The production adapter implements the same port over
/// the billing context's one-off charge capability (phase-04 extension) —
/// the saga's compensation logic (refund paths, idempotency on the order
/// id) is already exercised against this adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct SandboxPurchaseCharger;

#[async_trait::async_trait]
impl PurchaseCharger for SandboxPurchaseCharger {
    async fn charge(
        &self,
        _tenant_id: TenantId,
        order_id: sanvi_domains::domain::DomainOrderId,
        amount_minor: i64,
        currency: &str,
        description: &str,
    ) -> Result<String, DomainsError> {
        tracing::info!(
            order_id = %order_id,
            amount_minor,
            currency,
            description,
            "sandbox charge recorded"
        );
        Ok(format!("sandbox-ch-{order_id}"))
    }

    async fn refund(
        &self,
        _tenant_id: TenantId,
        order_id: sanvi_domains::domain::DomainOrderId,
        charge_id: &str,
    ) -> Result<(), DomainsError> {
        tracing::info!(order_id = %order_id, charge_id, "sandbox refund recorded");
        Ok(())
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
    Arc::new(LiveDomainLookupService::new(cache, domains, pool))
}

// ---------------------------------------------------------------------------
// Phase 09: the payments context's composition root. 09.1 assembles the
// provider registry — the single source of catalog entries — and wires
// the first vertical use case (registry → port → repo → outbox). The
// Stripe Connect adapter joins the registry with 09.2.
// ---------------------------------------------------------------------------

/// The phase-08 verified-domain allow-list for checkout redirect URLs,
/// read from the domains context's live-hostname projection (no RLS by
/// design — it is the resolution hot path). A host is allowed only when
/// it is live **and belongs to the requesting tenant**: another tenant's
/// verified domain is as useless here as an unverified one.
struct DomainsLiveAllowList {
    pool: sanvi_persistence::PgPool,
}

#[async_trait::async_trait]
impl sanvi_payments::TenantDomainAllowList for DomainsLiveAllowList {
    async fn allows(
        &self,
        tenant_id: TenantId,
        hostname: &str,
    ) -> Result<bool, sanvi_payments::PaymentsError> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT tenant_id FROM domains.live_hostnames WHERE normalized_hostname = $1",
        )
        .bind(hostname.trim_end_matches('.').to_ascii_lowercase())
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| sanvi_payments::PaymentsError::Storage(err.to_string()))?;
        Ok(row.map(|(id,)| id == tenant_id.as_i64()).unwrap_or(false))
    }
}

/// The payments context's read-only view of the platform's application-fee
/// configuration (the `platform_admin.application_fees` table). The
/// platform-admin context owns the write path and its API; this adapter
/// exists so checkout can consult the policy without the contexts
/// depending on each other. An absent row is "no fee configured".
struct PlatformApplicationFeeStore {
    tx_manager: sanvi_persistence::TxManager,
}

#[async_trait::async_trait]
impl sanvi_payments::domain::ports::ApplicationFeeStore for PlatformApplicationFeeStore {
    async fn configuration_for(
        &self,
        tenant_id: TenantId,
    ) -> Result<Option<sanvi_payments::domain::fees::FeeConfiguration>, sanvi_payments::PaymentsError>
    {
        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|err| sanvi_payments::PaymentsError::Storage(err.to_string()))?;
        let row: Option<(Option<i32>, Option<i64>, Option<String>, i64, bool)> = sqlx::query_as(
            "SELECT basis_points, fixed_amount_minor, fixed_currency, minimum_minor, enabled
             FROM platform_admin.application_fees
             WHERE tenant_id = $1 AND enabled",
        )
        .bind(tenant_id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| sanvi_payments::PaymentsError::Storage(err.to_string()))?;
        scope
            .rollback()
            .await
            .map_err(|err| sanvi_payments::PaymentsError::Storage(err.to_string()))?;
        Ok(row.map(
            |(bps, fixed_minor, fixed_currency, minimum_minor, _enabled)| {
                sanvi_payments::domain::fees::FeeConfiguration {
                    basis_points: bps.map(|v| u32::try_from(v.max(0) as i64).unwrap_or(0)),
                    fixed_minor,
                    fixed_currency,
                    minimum_minor,
                }
            },
        ))
    }
}

/// Build the payments context's application state.
///
/// The registry decides what exists: the fake provider is registered
/// only in non-production environments (gate 1), and the registry-backed
/// catalog independently refuses to list non-production descriptors in
/// production (gate 2) — a mistake in one place is not enough to leak a
/// test provider into a production catalog.
#[allow(clippy::too_many_arguments)]
pub fn build_payments_state(
    pool: sanvi_persistence::PgPool,
    ids: std::sync::Arc<dyn sanvi_kernel::RawIdGenerator>,
    config: &PaymentsConfig,
    registry: &prometheus::Registry,
    environment: &str,
    audit_writer: Arc<dyn sanvi_audit::domain::AuditWriter>,
) -> sanvi_payments::adapters::http::PaymentsState {
    let environment = RuntimeEnvironment::from_config(environment);

    let mut providers = PaymentsProviderRegistry::new();
    if environment == RuntimeEnvironment::NonProduction {
        providers
            .register(std::sync::Arc::new(FakeProvider::new()))
            .expect("the fake provider is the first registration");
    }
    // 09.2: the Stripe Connect adapter joins the registry when a Connect
    // restricted key is configured. Until then a production registry is
    // empty and the catalog answers an empty list — the honest state, not
    // a hardcoded one.
    if !config.connect_api_key.is_empty() {
        providers
            .register(std::sync::Arc::new(
                sanvi_payments::adapters::StripeConnectProvider::new(
                    config.connect_api_base.clone(),
                    config.connect_api_key.clone(),
                    config.stripe_api_version.clone(),
                    config.connect_webhook_secret.clone(),
                    config.webhook_tolerance_secs,
                ),
            ))
            .expect("the stripe connect adapter registers");
    }
    let providers = std::sync::Arc::new(providers);

    let catalog = std::sync::Arc::new(RegistryProviderCatalog::new(providers.clone(), environment));
    let connections: std::sync::Arc<dyn sanvi_payments::domain::ProviderConnectionRepository> =
        std::sync::Arc::new(SqlxProviderConnectionRepository::new());
    let events: std::sync::Arc<dyn sanvi_payments::domain::EventPublisher> =
        std::sync::Arc::new(PaymentsEvents::new(ids.clone()));
    let payment_ids: std::sync::Arc<dyn sanvi_payments::domain::IdGenerator> = std::sync::Arc::new(
        sanvi_payments::adapters::SystemIdGenerator::new(ids.clone()),
    );
    let tx = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let create_connection = std::sync::Arc::new(StartProviderConnection::new(
        tx.clone(),
        providers.clone(),
        connections.clone(),
        events,
        audit_writer.clone(),
        payment_ids.clone(),
    ));
    let account_session =
        std::sync::Arc::new(sanvi_payments::application::CreateAccountSession::new(
            tx.clone(),
            providers.clone(),
            connections.clone(),
            audit_writer.clone(),
        ));
    // 09.7: the guarded disconnect use case. Shares the read-model repos
    // with the tenant API so the money guards see exactly what the
    // dashboard sees.
    let disconnect_connection =
        std::sync::Arc::new(sanvi_payments::application::DisconnectConnection::new(
            tx.clone(),
            connections.clone(),
            std::sync::Arc::new(sanvi_payments::adapters::SqlxPaymentRepository::new()),
            std::sync::Arc::new(sanvi_payments::adapters::SqlxDisputeRepository::new()),
            std::sync::Arc::new(SqlxPayoutRepository::new()),
            std::sync::Arc::new(PaymentsEvents::new(ids.clone())),
            audit_writer.clone(),
        ));

    // 09.3: the Connect webhook transport. The endpoint's verifier carries
    // THIS endpoint's signing secret — Stripe's when the Connect adapter
    // is configured, else the fake provider's in non-production, else a
    // verifier that fails closed so an unconfigured endpoint rejects
    // everything. (The cross-secret invariant — never the phase-04
    // platform secret — is enforced at boot in AppConfig::validate.)
    let webhook_verifier: std::sync::Arc<dyn sanvi_payments::domain::WebhookVerifierPort> =
        if providers
            .get(sanvi_payments::domain::ProviderKind::StripeConnect)
            .is_some()
        {
            std::sync::Arc::new(sanvi_payments::adapters::StripeConnectProvider::new(
                config.connect_api_base.clone(),
                config.connect_api_key.clone(),
                config.stripe_api_version.clone(),
                config.connect_webhook_secret.clone(),
                config.webhook_tolerance_secs,
            ))
        } else if environment == RuntimeEnvironment::NonProduction {
            std::sync::Arc::new(sanvi_payments::adapters::FakeProvider::new())
        } else {
            std::sync::Arc::new(sanvi_payments::adapters::DenyAllVerifier::new())
        };
    let webhook_events: std::sync::Arc<dyn sanvi_payments::domain::WebhookEventRepository> =
        std::sync::Arc::new(sanvi_payments::adapters::SqlxWebhookEventRepository::new());
    let webhook_receiver = std::sync::Arc::new(sanvi_payments::application::WebhookReceiver::new(
        tx.clone(),
        webhook_events,
        webhook_verifier,
        config.webhook_allowed_clients.clone(),
    ));

    // 09.4: checkout on the connected account. The verified-domain
    // allow-list is the phase-08 projection; the fee policy reads the
    // platform-admin per-tenant configuration (which ships disabled —
    // no fee parameter leaves the process until an operator enables one);
    // the throttle is the in-process fixed window over the configured
    // per-minute budget (the platform's Redis limiters already ran ahead
    // of this handler; cross-instance hardening lands with 09.7).
    let checkouts: std::sync::Arc<dyn sanvi_payments::domain::CheckoutRepository> =
        std::sync::Arc::new(SqlxCheckoutRepository::new());
    let tax_settings: std::sync::Arc<dyn sanvi_payments::domain::TaxSettingsRepository> =
        std::sync::Arc::new(SqlxTaxSettingsRepository::new());
    let create_checkout = std::sync::Arc::new(CreateCheckout::new(
        tx.clone(),
        providers.clone(),
        connections.clone(),
        checkouts.clone(),
        std::sync::Arc::new(DomainsLiveAllowList { pool: pool.clone() }),
        std::sync::Arc::new(PaymentsEvents::new(ids.clone())),
        std::sync::Arc::new(ConfiguredApplicationFeePolicy::new(std::sync::Arc::new(
            PlatformApplicationFeeStore {
                tx_manager: tx.clone(),
            },
        ))),
        tax_settings.clone(),
        payment_ids.clone(),
    ));
    let checkout_throttle: std::sync::Arc<dyn sanvi_payments::domain::CheckoutThrottle> =
        std::sync::Arc::new(FixedWindowThrottle::new(
            config.checkout_rate_limit_per_minute,
        ));

    // 09.5: payments, refunds & disputes read models.
    let payments: std::sync::Arc<dyn sanvi_payments::domain::PaymentRepository> =
        std::sync::Arc::new(sanvi_payments::adapters::SqlxPaymentRepository::new());
    let refunds: std::sync::Arc<dyn sanvi_payments::domain::RefundRepository> =
        std::sync::Arc::new(sanvi_payments::adapters::SqlxRefundRepository::new());
    let disputes: std::sync::Arc<dyn sanvi_payments::domain::DisputeRepository> =
        std::sync::Arc::new(sanvi_payments::adapters::SqlxDisputeRepository::new());
    let list_payments = std::sync::Arc::new(sanvi_payments::application::ListPayments::new(
        tx.clone(),
        payments.clone(),
    ));
    let get_payment = std::sync::Arc::new(sanvi_payments::application::GetPaymentDetail::new(
        tx.clone(),
        payments.clone(),
        refunds.clone(),
        disputes.clone(),
        checkouts.clone(),
        connections.clone(),
    ));
    let issue_refund = std::sync::Arc::new(sanvi_payments::application::IssueRefund::new(
        tx.clone(),
        providers.clone(),
        payments.clone(),
        refunds.clone(),
        connections.clone(),
        checkouts.clone(),
        std::sync::Arc::new(PaymentsEvents::new(ids.clone())),
        audit_writer.clone(),
        payment_ids,
    ));
    let list_disputes = std::sync::Arc::new(sanvi_payments::application::ListDisputes::new(
        tx.clone(),
        disputes.clone(),
        payments.clone(),
        checkouts.clone(),
        connections.clone(),
    ));
    let export_payments = std::sync::Arc::new(sanvi_payments::application::ExportPayments::new(
        tx.clone(),
        payments.clone(),
        refunds.clone(),
        disputes.clone(),
        checkouts.clone(),
        audit_writer.clone(),
    ));
    // 09.6: payout visibility (read-only) and the tax-settings toggle
    // behind its compliance preflight.
    let payouts: std::sync::Arc<dyn sanvi_payments::domain::PayoutRepository> =
        std::sync::Arc::new(SqlxPayoutRepository::new());
    let list_payouts = std::sync::Arc::new(ListPayouts::new(
        tx.clone(),
        connections.clone(),
        providers.clone(),
    ));
    let manage_tax_settings = std::sync::Arc::new(ManageTaxSettings::new(
        tx.clone(),
        connections.clone(),
        providers.clone(),
        tax_settings.clone(),
    ));
    let fee_store: std::sync::Arc<dyn sanvi_payments::domain::ports::ApplicationFeeStore> =
        std::sync::Arc::new(PlatformApplicationFeeStore {
            tx_manager: tx.clone(),
        });

    sanvi_payments::adapters::http::PaymentsState {
        config: config.clone(),
        metrics: std::sync::Arc::new(PaymentsMetrics::new(registry)),
        list_providers: std::sync::Arc::new(ListProviders::new(catalog)),
        registry: providers,
        tx,
        connections,
        create_connection,
        account_session,
        disconnect_connection,
        webhook_receiver,
        checkouts,
        create_checkout,
        checkout_throttle,
        payments,
        refunds,
        disputes,
        list_payments,
        get_payment,
        issue_refund,
        list_disputes,
        export_payments,
        payouts,
        list_payouts,
        tax_settings,
        manage_tax_settings,
        fee_store,
    }
}

/// Phase 10's registry-derived capability catalog. The in-process adapter is
/// deliberately the only registered implementation until real adapters land.
///
/// `directive_resolver` and `subprocessors` come from the privacy context
/// (phase 05): the directive gate (TASK-014) is the phase's core
/// invariant, and advertising holds no directive or sub-processor state of
/// its own to consult instead.
pub async fn build_advertising_state(
    pool: sanvi_persistence::PgPool,
    ids: std::sync::Arc<dyn sanvi_kernel::RawIdGenerator>,
    config: &AdvertisingConfig,
    directive_resolver: Arc<dyn DirectiveResolver>,
    subprocessors: Arc<dyn sanvi_privacy::domain::SubProcessorRepository>,
) -> anyhow::Result<sanvi_advertising::adapters::http::AdvertisingState> {
    config.validate_oauth_enabled()?;
    config.validate_tracking_enabled()?;
    let token_kek = config.token_kek_bytes()?;
    let registry = std::sync::Arc::new(sanvi_advertising::adapters::rate_limited_registry(
        &FakeAdPlatform::registered_catalog(),
        |platform| match platform.as_str() {
            "meta" => (10, 2),
            _ => (
                config.quota_platform_wide_permits,
                config.quota_per_connection_permits,
            ),
        },
    ));
    let tx = sanvi_persistence::TxManager::new(pool.clone(), ids.clone());
    let oauth_store = std::sync::Arc::new(SqlxOAuthStateStore::new(tx.clone()));
    let oauth_state =
        OAuthStateManager::with_store(&config.oauth_state_signing_secret, oauth_store);
    let token_vault = std::sync::Arc::new(TokenVault::new(
        std::sync::Arc::new(SqlxTenantEnvelopeCrypto::new(
            token_kek,
            config.token_kek_id.clone(),
        )),
        std::sync::Arc::new(SqlxConnectionTokenRepository::new()),
    ));
    let campaigns: std::sync::Arc<dyn sanvi_advertising::domain::CampaignRepository> =
        std::sync::Arc::new(SqlxCampaignRepository::new());
    let creatives: std::sync::Arc<dyn sanvi_advertising::domain::CreativeRepository> =
        std::sync::Arc::new(SqlxCreativeRepository::new());
    let connections: std::sync::Arc<dyn sanvi_advertising::AdvertisingConnectionRepository> =
        std::sync::Arc::new(SqlxAdvertisingConnectionRepository::new());
    let events: std::sync::Arc<dyn sanvi_advertising::domain::EventPublisher> = std::sync::Arc::new(
        sanvi_advertising::adapters::OutboxEventPublisher::new(ids.clone()),
    );
    let advertising_ids: std::sync::Arc<dyn sanvi_advertising::domain::IdGenerator> =
        std::sync::Arc::new(sanvi_advertising::adapters::SystemIdGenerator::new(
            ids.clone(),
        ));

    // 10.5: conversion capture & the directive gate. The sub-processor
    // role snapshot is loaded once here rather than per beacon hit — see
    // `SnapshotVendorRoleLookup`'s own doc comment for why.
    let vendor_role: std::sync::Arc<dyn sanvi_advertising::domain::VendorRolePort> = {
        let subprocessor_tx = tx.clone();
        let mut subprocessor_scope = subprocessor_tx
            .begin_unscoped(
                "advertising: load sub-processor roles for the conversion tracking gate",
                "system",
            )
            .await?;
        std::sync::Arc::new(
            sanvi_advertising::adapters::SnapshotVendorRoleLookup::load(
                subprocessors.as_ref(),
                &mut subprocessor_scope,
            )
            .await?,
        )
    };
    let checkouts: std::sync::Arc<dyn sanvi_payments::domain::CheckoutRepository> =
        std::sync::Arc::new(sanvi_payments::adapters::SqlxCheckoutRepository::new());
    let payment_lookup: std::sync::Arc<dyn sanvi_advertising::domain::PaymentLookupPort> =
        std::sync::Arc::new(sanvi_advertising::adapters::CheckoutPaymentLookup::new(
            checkouts,
        ));
    let conversion_repo: std::sync::Arc<dyn sanvi_advertising::domain::ConversionEventRepository> =
        std::sync::Arc::new(SqlxCampaignRepository::new());
    let tracking_settings_repo: std::sync::Arc<
        dyn sanvi_advertising::domain::TrackingSettingsRepository,
    > = std::sync::Arc::new(sanvi_advertising::adapters::SqlxTrackingSettingsRepository::new());
    let capture = std::sync::Arc::new(
        sanvi_advertising::application::tracking::capture::CaptureConversion::new(
            tx.clone(),
            conversion_repo.clone(),
            payment_lookup,
            vendor_role,
            directive_resolver.clone(),
            advertising_ids.clone(),
        ),
    );
    let settings_mgr = std::sync::Arc::new(
        sanvi_advertising::application::tracking::settings::ManageTrackingSettings::new(
            tx.clone(),
            tracking_settings_repo,
        ),
    );
    let list_conversions = std::sync::Arc::new(
        sanvi_advertising::application::tracking::conversions::ListConversions::new(
            tx.clone(),
            conversion_repo.clone(),
        ),
    );
    let get_conversion_diagnostics = std::sync::Arc::new(
        sanvi_advertising::application::tracking::conversions::GetConversionDiagnostics::new(
            tx.clone(),
            conversion_repo.clone(),
        ),
    );
    let retry_conversion = std::sync::Arc::new(
        sanvi_advertising::application::tracking::retry::RetryConversion::new(
            tx.clone(),
            conversion_repo.clone(),
            directive_resolver.clone(),
        ),
    );
    let audience_repo: std::sync::Arc<dyn sanvi_advertising::domain::AudienceRepository> =
        std::sync::Arc::new(SqlxCampaignRepository::new());
    let mut audience_sinks: std::collections::BTreeMap<
        sanvi_advertising::domain::PlatformKey,
        std::sync::Arc<dyn sanvi_advertising::domain::AudienceSyncPort>,
    > = std::collections::BTreeMap::new();
    for descriptor in sanvi_advertising::adapters::FakeAdPlatform::fixtures() {
        audience_sinks.insert(
            descriptor.key.clone(),
            std::sync::Arc::new(sanvi_advertising::adapters::FakeAudienceSync::new(
                descriptor,
            )),
        );
    }
    let advertising_metrics =
        std::sync::Arc::new(sanvi_advertising::adapters::AdvertisingMetrics::default());
    let create_audience = std::sync::Arc::new(
        sanvi_advertising::application::tracking::audiences::CreateAudience::new(
            tx.clone(),
            audience_repo.clone(),
            advertising_ids.clone(),
        ),
    );
    let list_audiences = std::sync::Arc::new(
        sanvi_advertising::application::tracking::audiences::ListAudiences::new(
            tx.clone(),
            audience_repo.clone(),
        ),
    );
    let refresh_audience = std::sync::Arc::new(
        sanvi_advertising::application::tracking::audiences::RefreshAudience::new(
            tx.clone(),
            audience_repo,
            conversion_repo,
            connections.clone(),
            directive_resolver,
            audience_sinks,
            advertising_metrics.clone(),
        ),
    );

    let budget_caps_repo =
        std::sync::Arc::new(sanvi_advertising::adapters::SqlxBudgetCapRepository::new());
    let budget_alerts_repo =
        std::sync::Arc::new(sanvi_advertising::adapters::SqlxBudgetAlertRepository::new());
    let fx_rates = std::sync::Arc::new(sanvi_advertising::adapters::UnavailableFxRate::new());
    let metrics_repo =
        std::sync::Arc::new(sanvi_advertising::adapters::SqlxMetricsRepository::new());
    let pause_campaign_service =
        std::sync::Arc::new(sanvi_advertising::application::PauseCampaign::new(
            tx.clone(),
            campaigns.clone(),
            connections.clone(),
            events.clone(),
            registry.clone(),
            advertising_ids.clone(),
        ));
    let notifier = std::sync::Arc::new(sanvi_advertising::adapters::LoggingBudgetNotifier::new());
    let spend_status_service =
        std::sync::Arc::new(sanvi_advertising::application::GetSpendStatus::new(
            tx.clone(),
            metrics_repo.clone(),
            budget_caps_repo.clone(),
            campaigns.clone(),
            connections.clone(),
            fx_rates,
            config.metrics_restatement_days,
        ));
    let evaluator = std::sync::Arc::new(sanvi_advertising::application::ThresholdEvaluator::new(
        tx.clone(),
        budget_caps_repo.clone(),
        budget_alerts_repo.clone(),
        campaigns.clone(),
        connections.clone(),
        pause_campaign_service.clone(),
        spend_status_service.clone(),
        events.clone(),
        advertising_ids.clone(),
        notifier,
        advertising_metrics.clone(),
    ));
    let get_budget_caps = std::sync::Arc::new(sanvi_advertising::application::GetBudgetCaps::new(
        tx.clone(),
        budget_caps_repo.clone(),
    ));
    let put_budget_cap = std::sync::Arc::new(sanvi_advertising::application::PutBudgetCap::new(
        tx.clone(),
        budget_caps_repo.clone(),
        advertising_ids.clone(),
        spend_status_service.clone(),
        evaluator,
    ));
    let get_campaign_budget_cap = std::sync::Arc::new(
        sanvi_advertising::application::GetCampaignBudgetCap::new(tx.clone(), budget_caps_repo),
    );
    let list_budget_alerts =
        std::sync::Arc::new(sanvi_advertising::application::ListBudgetAlerts::new(
            tx.clone(),
            budget_alerts_repo.clone(),
        ));
    let acknowledge_budget_alert = std::sync::Arc::new(
        sanvi_advertising::application::AcknowledgeBudgetAlert::new(tx.clone(), budget_alerts_repo),
    );

    Ok(sanvi_advertising::adapters::http::AdvertisingState {
        config: config.clone(),
        idempotency: sanvi_persistence::IdempotencyStore::new(tx.pool().clone()),
        catalog: std::sync::Arc::new(ListPlatformCatalog::new(registry.clone())),
        reconcile_campaign: std::sync::Arc::new(ReconcileCampaign::new(
            tx.clone(),
            campaigns.clone(),
            events.clone(),
            advertising_ids.clone(),
        )),
        resolve_campaign_drift: std::sync::Arc::new(ResolveCampaignDrift::new(
            tx.clone(),
            campaigns.clone(),
            connections.clone(),
            events.clone(),
            registry.clone(),
            advertising_ids.clone(),
        )),
        create_campaign: std::sync::Arc::new(CreateCampaign::new(
            tx.clone(),
            campaigns.clone(),
            connections.clone(),
            registry.clone(),
            events.clone(),
            advertising_ids.clone(),
        )),
        get_campaign: std::sync::Arc::new(GetCampaign::new(tx.clone(), campaigns.clone())),
        list_campaigns: std::sync::Arc::new(ListCampaigns::new(tx.clone(), campaigns.clone())),
        validate_campaign: std::sync::Arc::new(ValidateCampaign::new(
            tx.clone(),
            campaigns.clone(),
            creatives.clone(),
            registry.clone(),
        )),
        patch_campaign: std::sync::Arc::new(PatchCampaign::new(
            tx.clone(),
            campaigns.clone(),
            connections.clone(),
            creatives.clone(),
            events.clone(),
            registry.clone(),
            advertising_ids.clone(),
        )),
        publish_campaign: std::sync::Arc::new(PublishCampaign::new(
            tx.clone(),
            campaigns.clone(),
            connections.clone(),
            creatives.clone(),
            events.clone(),
            registry.clone(),
            advertising_ids.clone(),
        )),
        pause_campaign: pause_campaign_service,
        resume_campaign: std::sync::Arc::new(ResumeCampaign::new(
            tx.clone(),
            campaigns.clone(),
            connections.clone(),
            events.clone(),
            registry.clone(),
            advertising_ids.clone(),
        )),
        list_campaign_changes: std::sync::Arc::new(ListCampaignChanges::new(
            tx.clone(),
            campaigns.clone(),
        )),
        add_ad_group: std::sync::Arc::new(AddAdGroup::new(
            tx.clone(),
            campaigns.clone(),
            connections.clone(),
            events.clone(),
            registry.clone(),
            advertising_ids.clone(),
        )),
        update_ad_group: std::sync::Arc::new(UpdateAdGroup::new(
            tx.clone(),
            campaigns.clone(),
            connections.clone(),
            events.clone(),
            registry.clone(),
            advertising_ids.clone(),
        )),
        add_ad: std::sync::Arc::new(AddAd::new(
            tx.clone(),
            campaigns.clone(),
            connections.clone(),
            creatives.clone(),
            events.clone(),
            registry.clone(),
            advertising_ids.clone(),
        )),
        update_ad: std::sync::Arc::new(UpdateAd::new(
            tx.clone(),
            campaigns,
            connections.clone(),
            creatives.clone(),
            events,
            registry.clone(),
            advertising_ids.clone(),
        )),
        start_authorization: std::sync::Arc::new(StartAuthorization::new(
            registry.clone(),
            oauth_state.clone(),
            advertising_ids.clone(),
        )),
        handle_callback: std::sync::Arc::new(HandleCallback::new(
            tx.clone(),
            connections.clone(),
            registry.clone(),
            oauth_state.clone(),
            token_vault,
            advertising_ids.clone(),
        )),
        create_connection: std::sync::Arc::new(CreateConnection::new(
            tx.clone(),
            connections.clone(),
        )),
        list_connections: std::sync::Arc::new(ListConnections::new(
            tx.clone(),
            connections.clone(),
            registry.clone(),
        )),
        delete_connection: std::sync::Arc::new(DeleteConnection::new(
            tx.clone(),
            connections.clone(),
        )),
        create_creative: std::sync::Arc::new(CreateCreative::new(
            tx.clone(),
            creatives.clone(),
            connections.clone(),
            registry.clone(),
            advertising_ids.clone(),
        )),
        get_creative: std::sync::Arc::new(GetCreative::new(tx.clone(), creatives.clone())),
        list_creatives: std::sync::Arc::new(ListCreatives::new(tx.clone(), creatives.clone())),
        delete_creative: std::sync::Arc::new(DeleteCreative::new(tx.clone(), creatives.clone())),
        get_creative_previews: std::sync::Arc::new(GetCreativePreviews::new(
            tx.clone(),
            creatives,
            registry.clone(),
        )),
        query_metrics: std::sync::Arc::new(QueryMetrics::new(
            tx.clone(),
            metrics_repo,
            config.metrics_max_query_span_days,
            config.metrics_restatement_days,
        )),
        get_freshness: std::sync::Arc::new(GetFreshness::new(
            tx.clone(),
            connections.clone(),
            config.metrics_freshness_stalled_after_hours,
        )),
        get_platform_ads_health: std::sync::Arc::new(
            sanvi_advertising::application::GetPlatformAdsHealth::new(
                tx,
                connections,
                registry,
                config.metrics_freshness_stalled_after_hours,
            ),
        ),
        get_budget_caps,
        put_budget_cap,
        get_campaign_budget_cap,
        list_budget_alerts,
        acknowledge_budget_alert,
        get_spend_status: spend_status_service,
        oauth_state,
        // Concurrency caps on the public track beacon: a coarse abuse
        // control (see `tracking_handlers` module doc), not a
        // time-windowed rate limiter — deliberately fixed rather than
        // configurable, since there is no per-tenant knob for a
        // pre-authentication endpoint.
        tracking_ip_limiter: std::sync::Arc::new(sanvi_advertising::adapters::QuotaLimiter::new(
            500, 5,
        )),
        tracking_domain_limiter: std::sync::Arc::new(
            sanvi_advertising::adapters::QuotaLimiter::new(1000, 50),
        ),
        capture,
        settings_mgr,
        list_conversions,
        get_conversion_diagnostics,
        retry_conversion,
        create_audience,
        list_audiences,
        refresh_audience,
    })
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

    let registrar: Arc<dyn sanvi_domains::domain::DomainRegistrarPort> =
        Arc::new(SandboxRegistrar::new(
            config.registrar.registrar_id.clone(),
            &config.registrar.tlds,
        ));
    let provider: Arc<dyn sanvi_domains::domain::DnsProviderPort> =
        Arc::new(SqlxDnsProvider::new(records.clone()));
    let ca: Arc<dyn sanvi_domains::domain::CertificateAuthorityPort> = Arc::new(
        SandboxCertificateAuthority::new(config.cert_lifetime_days, ids.clone()),
    );
    let charger: Arc<dyn PurchaseCharger> = Arc::new(SandboxPurchaseCharger);
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
    payments: Option<sanvi_payments::adapters::http::PaymentsState>,
    advertising: Option<sanvi_advertising::adapters::http::AdvertisingState>,
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
    let tenant_scope = match &domains {
        Some(_) => tenant_scope.configure(domains_routes::domains_tenant_scope_routes),
        None => tenant_scope,
    };
    let tenant_scope = if config.features.payments {
        match &payments {
            Some(_) => tenant_scope.configure(payments_routes::payments_tenant_scope_routes),
            None => tenant_scope,
        }
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
    let app = match billing {
        Some(billing_state) => app
            .app_data(web::Data::new(billing_state))
            .configure(billing_routes::public_routes)
            .configure(billing_routes::billing_platform_routes)
            .configure(billing_routes::billing_internal_routes),
        None => app,
    };

    let app = match advertising {
        Some(state) => app
            .app_data(web::Data::new(state))
            .configure(advertising_routes::tracking_public_routes)
            .configure(advertising_routes::advertising_platform_routes),
        None => app,
    };

    // Phase-05 gate: privacy routes (subject-scoped scope wrapped with the
    // GPC middleware, public disclosures, platform control plane).
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
        None => app,
    };

    // Phase-09: tenant routes remain behind `features.payments`, while the
    // signature-authenticated internal webhook receiver is always wired.
    // `/internal` sits outside the authentication middleware by design.
    let app = match payments {
        Some(state) => app
            .app_data(web::Data::new(state))
            .configure(payments_routes::payments_internal_routes),
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
                vec![
                    "/healthz".to_string(),
                    "/readyz".to_string(),
                    "/metrics".to_string(),
                    "/openapi.json".to_string(),
                    "/scalar".to_string(),
                    "/api/v1/public".to_string(),
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

    middleware::stack(
        app,
        server_config,
        redis,
        tenant_middleware,
        authn,
        impersonation,
        locale_middleware,
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
                extensions: None,
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
