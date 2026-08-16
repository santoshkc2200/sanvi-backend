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
use sanvi_access::{
    AccessConfig,
    adapters::{HttpKetoClient, InMemoryDecisionCache, SqlxMembershipSource, SqlxRoleRepository},
    application::{CachedAuthorizer, CheckPermission, ListPermissions, RebuildAccessTuples},
    domain::{Authorizer, DecisionCache, PermissionStore, RoleRepository},
};
use sanvi_http::{
    AuthnMiddleware, PermissionGate, ProblemDetail, ResourceRef, Subject, middleware,
};
use sanvi_identity::{
    IdentityConfig,
    adapters::{
        HttpKratosClient, KratosSessionVerifier, LoggingNotifications, OutboxEventPublisher,
        RedisSessionCache, SystemClock, SystemIdGenerator,
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
    let events: Arc<dyn EventPublisher> = Arc::new(OutboxEventPublisher::new());
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

/// The access context's gate + state, implementing the platform's
/// `PermissionGate` port.
#[derive(Clone)]
pub struct AccessWiring {
    pub gate: Arc<dyn PermissionGate>,
    pub state: sanvi_access::adapters::http::AccessState,
    pub authorizer: Arc<dyn Authorizer>,
    pub role_repository: Arc<dyn RoleRepository>,
    pub permission_store: Arc<dyn PermissionStore>,
    pub decision_cache: Arc<dyn DecisionCache>,
    pub tx_manager: sanvi_persistence::TxManager,
    pub rebuild: Arc<RebuildAccessTuples>,
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

/// Build the access context's application state.
pub fn build_access_state(
    pool: sanvi_persistence::PgPool,
    config: &AccessConfig,
    keto_read_url: &str,
    keto_write_url: &str,
    memberships: Arc<dyn MembershipRepository>,
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

    AccessWiring {
        gate,
        state: sanvi_access::adapters::http::AccessState {
            list_permissions: Arc::new(ListPermissions::new()),
        },
        authorizer,
        role_repository: roles,
        permission_store: store,
        decision_cache: cache,
        tx_manager,
        rebuild,
    }
}

/// Assemble the actix `App`: middleware stack, routes, OpenAPI, metrics.
///
/// This is the factory `HttpServer` clones per worker; everything captured
/// is cheap to clone (pools, `Arc`s, plain data).
pub fn build_app(
    config: AppConfig,
    metrics: Arc<MetricsRegistry>,
    system: SystemState,
    tenancy: TenancyState,
    identity: IdentityWiring,
    access: AccessWiring,
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

    let app = App::new()
        .app_data(web::Data::new(state))
        .app_data(web::Data::new(system))
        .app_data(web::Data::new(tenancy.clone()))
        .app_data(web::Data::new(identity.state.clone()))
        .app_data(web::Data::new(access.state.clone()))
        .app_data(web::Data::new(identity.webhook.as_ref().clone()))
        .app_data(web::Data::new(access.gate.clone()))
        .app_data(web::JsonConfig::default().limit(server_config.max_body_bytes))
        .app_data(web::PayloadConfig::default().limit(server_config.max_body_bytes))
        .configure(health_routes)
        .configure(platform_routes)
        .configure(tenancy_routes::platform_routes)
        .service(
            web::scope("/api/v1/tenant")
                .configure(tenancy_routes::tenant_scope_routes)
                .configure(identity_routes::tenant_scope_routes),
        )
        .configure(tenancy_routes::public_routes)
        .configure(identity_routes::identity_routes)
        .configure(identity_routes::internal_routes)
        .configure(sanvi_access::routes::access_routes)
        .service(web::scope("/api/v1/system").configure(api_routes));

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
            exempt_prefixes: vec![
                "/healthz".to_string(),
                "/readyz".to_string(),
                "/metrics".to_string(),
                "/openapi.json".to_string(),
                "/scalar".to_string(),
                "/api/v1/public".to_string(),
                "/internal".to_string(),
            ],
        }))
    } else {
        None
    };

    middleware::stack(app, server_config, redis, tenant_middleware, authn)
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
