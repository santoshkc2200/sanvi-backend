//! Impersonation use-case tests (real Postgres): guardrails on
//! creation, immediate revocation, membership validation.

use std::sync::Arc;

use sanvi_audit::{adapters::SqlxAuditWriter, domain::AuditWriter};
use sanvi_identity::domain::MembershipRepository;
use sanvi_persistence::TxManager;
use sanvi_platform_admin::{
    adapters::{
        NoopPrivacyLock, OutboxEventPublisher, SqlxImpersonationGrantRepository, SystemClock,
        SystemIdGenerator,
    },
    application::{
        CreateImpersonation, CreateImpersonationCommand, ListImpersonations, RevokeImpersonation,
    },
    domain::{
        EventPublisher, IdGenerator, ImpersonationGrantId, ImpersonationGrantRepository,
        ImpersonationMode, PlatformAdminError, PrivacyLock,
    },
};
use sanvi_testsupport::TestDb;
use uuid::Uuid;

async fn prepare() -> (sanvi_persistence::PgPool, sanvi_persistence::PgPool, TestDb) {
    let db = sanvi_testsupport::test_db().await;
    let app = db.app_pool();
    let migrator = db.migrator_pool().await;
    (app, migrator, db)
}

/// Insert a tenant + active member directly (identity projection rows).
/// The migrator role has no RLS bypass, so the escape hatch GUC is set
/// first — exactly like the app's audited PlatformScope.
async fn insert_tenant_and_member(
    migrator: &sanvi_persistence::PgPool,
    tenant_id: Uuid,
    slug: &str,
    user_id: Uuid,
    email: &str,
) {
    let mut tx = migrator.begin().await.expect("owner tx");
    sqlx::query("SELECT set_config('sanvi.platform_scope', 'on', true)")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO tenancy.tenants
           (id, slug, display_name, status, region, default_locale, created_at, updated_at)
         VALUES ($1, $2, $3, 'active', 'us', 'en', now(), now())",
    )
    .bind(tenant_id)
    .bind(slug)
    .bind(format!("Tenant {slug}"))
    .execute(&mut *tx)
    .await
    .expect("tenant row");

    sqlx::query(
        "INSERT INTO identity.users (id, kratos_identity_id, primary_email, email_verified, status)
         VALUES ($1, $2, $3, true, 'active')",
    )
    .bind(user_id)
    .bind(Uuid::now_v7())
    .bind(email)
    .execute(&mut *tx)
    .await
    .expect("user row");

    sqlx::query(
        "INSERT INTO identity.memberships (tenant_id, user_id, status, created_at)
         VALUES ($1, $2, 'active', now())",
    )
    .bind(tenant_id)
    .bind(user_id)
    .execute(&mut *tx)
    .await
    .expect("membership row");
    tx.commit().await.expect("seed commit");
}

struct Wired {
    create: Arc<CreateImpersonation>,
    list: Arc<ListImpersonations>,
    revoke: Arc<RevokeImpersonation>,
}

fn wire(pool: sanvi_persistence::PgPool, privacy: Arc<dyn PrivacyLock>) -> Wired {
    let tx_manager = TxManager::new(pool);
    let grants: Arc<dyn ImpersonationGrantRepository> =
        Arc::new(SqlxImpersonationGrantRepository::new());
    let memberships: Arc<dyn MembershipRepository> =
        Arc::new(sanvi_identity::adapters::repo::SqlxMembershipRepository::new());
    let events: Arc<dyn EventPublisher> = Arc::new(OutboxEventPublisher::new());
    let ids: Arc<dyn IdGenerator> = Arc::new(SystemIdGenerator);
    let clock = Arc::new(SystemClock);
    let audit: Arc<dyn AuditWriter> = Arc::new(SqlxAuditWriter::new());

    Wired {
        create: Arc::new(CreateImpersonation::new(
            tx_manager.clone(),
            grants.clone(),
            memberships,
            privacy,
            events.clone(),
            ids,
            clock.clone(),
            audit.clone(),
            30,
        )),
        list: Arc::new(ListImpersonations::new(
            tx_manager.clone(),
            grants.clone(),
            clock.clone(),
        )),
        revoke: Arc::new(RevokeImpersonation::new(
            tx_manager, grants, events, clock, audit,
        )),
    }
}

#[tokio::test]
async fn grants_are_created_listed_and_revoked_immediately() {
    let (app, migrator, _node) = prepare().await;
    let tenant = Uuid::now_v7();
    let target = Uuid::now_v7();
    insert_tenant_and_member(
        &migrator,
        tenant,
        "impersonation",
        target,
        "target@example.com",
    )
    .await;
    let actor = Uuid::now_v7();

    let wired = wire(app, Arc::new(NoopPrivacyLock));
    let grant = wired
        .create
        .execute(
            actor,
            None,
            CreateImpersonationCommand {
                tenant_id: tenant,
                target_user_id: target,
                reason: "investigating support ticket #123".to_string(),
                mode: ImpersonationMode::ReadOnly,
                duration_minutes: 15,
            },
        )
        .await
        .expect("create grant");
    assert_eq!(grant.actor_id, actor);
    assert_eq!(grant.mode, ImpersonationMode::ReadOnly);
    assert!(grant.revoked_at.is_none());

    // Active grants list includes it.
    let active = wired.list.execute(Some(tenant), 100).await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, grant.id);

    // Revocation is immediate: the same list query no longer sees it.
    wired
        .revoke
        .execute(
            Uuid::now_v7(),
            None,
            ImpersonationGrantId::from_uuid(grant.id),
        )
        .await
        .expect("revoke");
    let active = wired.list.execute(Some(tenant), 100).await.unwrap();
    assert!(active.is_empty());

    // Idempotent: revoking again succeeds.
    wired
        .revoke
        .execute(
            Uuid::now_v7(),
            None,
            ImpersonationGrantId::from_uuid(grant.id),
        )
        .await
        .expect("idempotent revoke");
}

#[tokio::test]
async fn creation_guardrails_hold() {
    let (app, migrator, _node) = prepare().await;
    let tenant = Uuid::now_v7();
    let target = Uuid::now_v7();
    insert_tenant_and_member(&migrator, tenant, "guards", target, "guarded@example.com").await;

    let wired = wire(app, Arc::new(NoopPrivacyLock));
    let base = CreateImpersonationCommand {
        tenant_id: tenant,
        target_user_id: target,
        reason: "support".to_string(),
        mode: ImpersonationMode::ReadOnly,
        duration_minutes: 10,
    };

    // Missing reason.
    let err = wired
        .create
        .execute(
            Uuid::now_v7(),
            None,
            CreateImpersonationCommand {
                reason: "   ".to_string(),
                ..base.clone()
            },
        )
        .await
        .expect_err("reason required");
    assert!(matches!(err, PlatformAdminError::MissingReason));

    // Duration above the 30-minute ceiling.
    let err = wired
        .create
        .execute(
            Uuid::now_v7(),
            None,
            CreateImpersonationCommand {
                duration_minutes: 31,
                ..base.clone()
            },
        )
        .await
        .expect_err("duration capped");
    assert!(matches!(
        err,
        PlatformAdminError::DurationTooLong { max_minutes: 30 }
    ));

    // The target must be an active member of the tenant.
    let err = wired
        .create
        .execute(
            Uuid::now_v7(),
            None,
            CreateImpersonationCommand {
                target_user_id: Uuid::now_v7(),
                ..base.clone()
            },
        )
        .await
        .expect_err("target must be a member");
    assert!(matches!(err, PlatformAdminError::TargetNotMember { .. }));

    // The tenant must exist.
    let err = wired
        .create
        .execute(
            Uuid::now_v7(),
            None,
            CreateImpersonationCommand {
                tenant_id: Uuid::now_v7(),
                ..base
            },
        )
        .await
        .expect_err("tenant must exist");
    assert!(matches!(err, PlatformAdminError::TargetNotMember { .. }));
}

/// A privacy lock that always locks (phase 05 wires the real one; the
/// refusal path is proven here).
struct LockedPrivacy;

#[async_trait::async_trait]
impl PrivacyLock for LockedPrivacy {
    async fn is_locked(&self, _tenant_id: Uuid) -> Result<bool, PlatformAdminError> {
        Ok(true)
    }
}

#[tokio::test]
async fn impersonation_into_a_privacy_locked_tenant_is_refused() {
    let (app, migrator, _node) = prepare().await;
    let tenant = Uuid::now_v7();
    let target = Uuid::now_v7();
    insert_tenant_and_member(&migrator, tenant, "locked", target, "locked@example.com").await;

    let wired = wire(app.clone(), Arc::new(LockedPrivacy));
    let err = wired
        .create
        .execute(
            Uuid::now_v7(),
            None,
            CreateImpersonationCommand {
                tenant_id: tenant,
                target_user_id: target,
                reason: "support".to_string(),
                mode: ImpersonationMode::ReadOnly,
                duration_minutes: 10,
            },
        )
        .await
        .expect_err("privacy lock must refuse impersonation");
    assert!(matches!(err, PlatformAdminError::PrivacyLocked { .. }));

    // And nothing was written.
    let tx_manager = TxManager::new(app);
    let grants = SqlxImpersonationGrantRepository::new();
    let mut scope = tx_manager
        .begin_unscoped("test_list", "test")
        .await
        .unwrap();
    let active = grants
        .list_active(&mut scope, Some(tenant), chrono::Utc::now(), 100)
        .await
        .unwrap();
    assert!(active.is_empty());
    scope.commit().await.unwrap();
}
