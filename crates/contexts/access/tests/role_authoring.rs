//! Role authoring escalation tests: the "cannot grant what you lack"
//! invariant, scope mismatch rejection, and system-role immutability —
//! over in-memory fakes (no infrastructure).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use sanvi_access::{
    application::{
        CreateCustomRole, CreateCustomRoleCommand, DeleteCustomRole, UpdateCustomRole,
        UpdateCustomRoleCommand,
    },
    domain::{
        AccessError, Authorizer, Clock, Decision, EventPublisher, IdGenerator, ResourceRef, Role,
        RoleId, RoleRepository, RoleScope,
    },
};
use sanvi_audit::domain::{AuditEntryData, AuditError, AuditWriter};
use sanvi_persistence::{DatabaseConfig, Migrator, PgPool, PlatformScope, TxManager, build_pool};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

fn migrations_dir() -> String {
    format!("{}/../../../migrations", env!("CARGO_MANIFEST_DIR"))
}

/// Fresh database with all migrations applied; returns app-role and
/// owner-role pools (and the container handle, which must stay alive).
async fn prepare() -> (PgPool, PgPool, testcontainers::ContainerAsync<Postgres>) {
    let node = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres");
    let port = node.get_host_port_ipv4(5432).await.expect("postgres port");
    let admin = build_pool(&DatabaseConfig {
        host: "localhost".to_string(),
        port,
        name: "postgres".to_string(),
        user: "postgres".to_string(),
        password: "postgres".to_string(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("admin pool");
    for statement in [
        "CREATE ROLE sanvi_migrator LOGIN PASSWORD 'migrator' NOSUPERUSER NOCREATEDB NOCREATEROLE",
        "CREATE ROLE sanvi_app LOGIN PASSWORD 'app' NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS",
        "CREATE DATABASE sanvi_test OWNER sanvi_migrator",
    ] {
        sqlx::query(statement)
            .execute(&admin)
            .await
            .expect("role/database setup");
    }
    let migrator = build_pool(&DatabaseConfig {
        host: "localhost".to_string(),
        port,
        name: "sanvi_test".to_string(),
        user: "sanvi_migrator".to_string(),
        password: "migrator".to_string(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("migrator pool");
    Migrator::new(migrations_dir())
        .run(&migrator)
        .await
        .expect("migrations up");
    let app = build_pool(&DatabaseConfig {
        host: "localhost".to_string(),
        port,
        name: "sanvi_test".to_string(),
        user: "sanvi_app".to_string(),
        password: "app".to_string(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("app pool");
    (app, migrator, node)
}

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

struct MemoryRoles {
    inner: Mutex<Vec<Role>>,
}

impl MemoryRoles {
    fn new() -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
        }
    }

    fn seed_system(&self) {
        let mut roles = self.inner.lock().unwrap();
        roles.push(Role::from_parts(
            RoleId::new(),
            RoleScope::Tenant,
            None,
            "tenant_owner".to_string(),
            "Tenant Owner".to_string(),
            BTreeSet::new(),
            true,
            Utc::now(),
        ));
    }
}

#[async_trait]
impl RoleRepository for MemoryRoles {
    async fn save(&self, _scope: &mut PlatformScope<'_>, role: &Role) -> Result<(), AccessError> {
        let mut roles = self.inner.lock().unwrap();
        if let Some(existing) = roles.iter_mut().find(|r| r.id() == role.id()) {
            *existing = role.clone();
        } else {
            roles.push(role.clone());
        }
        Ok(())
    }

    async fn find_by_key(
        &self,
        _scope: &mut PlatformScope<'_>,
        scope_kind: RoleScope,
        tenant_id: Option<Uuid>,
        key: &str,
    ) -> Result<Option<Role>, AccessError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.scope() == scope_kind && r.tenant_id() == tenant_id && r.key() == key)
            .cloned())
    }

    async fn find_by_id(
        &self,
        _scope: &mut PlatformScope<'_>,
        id: RoleId,
    ) -> Result<Option<Role>, AccessError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.id() == id)
            .cloned())
    }

    async fn delete(&self, _scope: &mut PlatformScope<'_>, id: RoleId) -> Result<(), AccessError> {
        self.inner.lock().unwrap().retain(|r| r.id() != id);
        Ok(())
    }

    async fn granting(
        &self,
        _scope: &mut PlatformScope<'_>,
        _permission: &str,
        _tenant_id: Option<Uuid>,
    ) -> Result<Vec<Role>, AccessError> {
        Ok(Vec::new())
    }

    async fn list_all(&self, _scope: &mut PlatformScope<'_>) -> Result<Vec<Role>, AccessError> {
        Ok(self.inner.lock().unwrap().clone())
    }

    async fn assignable_to(
        &self,
        _scope: &mut PlatformScope<'_>,
        _tenant_id: Uuid,
    ) -> Result<Vec<Role>, AccessError> {
        Ok(Vec::new())
    }
}

/// The authorizer answers "does the actor hold this permission in this
/// tenant" from a fixed allow-list.
struct FixedAuthorizer {
    held: Vec<String>,
}

#[async_trait]
impl Authorizer for FixedAuthorizer {
    async fn check(
        &self,
        _user_id: Uuid,
        permission: &sanvi_access::domain::Permission,
        _resource: &ResourceRef,
    ) -> Result<Decision, AccessError> {
        Ok(if self.held.iter().any(|p| p == permission.as_str()) {
            Decision::Allowed
        } else {
            Decision::Denied
        })
    }
}

struct NoopEvents;

#[async_trait]
impl EventPublisher for NoopEvents {
    async fn publish(
        &self,
        _scope: &mut PlatformScope<'_>,
        _events: &[sanvi_access::domain::AccessEvent],
    ) -> Result<(), AccessError> {
        Ok(())
    }

    async fn publish_scoped(
        &self,
        _scope: &mut sanvi_persistence::ScopedTx<'_>,
        _events: &[sanvi_access::domain::AccessEvent],
    ) -> Result<(), AccessError> {
        Ok(())
    }
}

struct SeqIds(std::sync::atomic::AtomicU64);

impl SeqIds {
    fn new() -> Self {
        Self(std::sync::atomic::AtomicU64::new(0))
    }
}

impl IdGenerator for SeqIds {
    fn next_role_id(&self) -> RoleId {
        let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut bytes = [0u8; 16];
        bytes[8] = 0x70; // version 7 for realism
        bytes[0..8].copy_from_slice(&n.to_be_bytes());
        RoleId::from_uuid(Uuid::from_bytes(bytes))
    }
}

struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> chrono::DateTime<Utc> {
        "2026-08-16T12:00:00Z".parse().unwrap()
    }
}

struct NoopAudit;

#[async_trait]
impl AuditWriter for NoopAudit {
    async fn append(
        &self,
        _scope: &mut PlatformScope<'_>,
        _data: &AuditEntryData,
    ) -> Result<i64, AuditError> {
        Ok(1)
    }
}

fn create_role(
    pool: PgPool,
    roles: Arc<MemoryRoles>,
    held: Vec<String>,
    events: Arc<NoopEvents>,
    ids: Arc<SeqIds>,
) -> CreateCustomRole {
    CreateCustomRole::new(
        TxManager::new(pool),
        roles,
        Arc::new(FixedAuthorizer { held }),
        events,
        ids,
        Arc::new(FixedClock),
        Arc::new(NoopAudit),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_actor_can_create_a_role_with_permissions_they_hold() {
    let (app, _migrator, _node) = prepare().await;
    let roles = Arc::new(MemoryRoles::new());
    let use_case = create_role(
        app,
        roles.clone(),
        vec!["identity.member.read".to_string()],
        Arc::new(NoopEvents),
        Arc::new(SeqIds::new()),
    );
    let tenant = Uuid::now_v7();
    let role = use_case
        .execute(
            Uuid::now_v7(),
            None,
            CreateCustomRoleCommand {
                tenant_id: tenant,
                key: "support_agent".to_string(),
                name: "Support Agent".to_string(),
                permissions: vec!["identity.member.read".to_string()],
            },
        )
        .await
        .expect("create role");
    assert_eq!(role.key(), "support_agent");
    assert!(role.permissions().contains("identity.member.read"));
}

#[tokio::test]
async fn an_actor_cannot_grant_a_permission_they_lack() {
    // Negative path: rejects before any transaction — the lazy pool that
    // cannot connect proves storage is never touched.
    let roles = Arc::new(MemoryRoles::new());
    let use_case = create_role(
        PgPool::connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
            .expect("lazy pool construction never connects"),
        roles.clone(),
        vec!["identity.member.read".to_string()],
        Arc::new(NoopEvents),
        Arc::new(SeqIds::new()),
    );
    let err = use_case
        .execute(
            Uuid::now_v7(),
            None,
            CreateCustomRoleCommand {
                tenant_id: Uuid::now_v7(),
                key: "escalator".to_string(),
                name: "Escalator".to_string(),
                permissions: vec!["identity.member.remove".to_string()],
            },
        )
        .await
        .expect_err("must reject escalation");
    assert!(matches!(
        err,
        AccessError::Escalation { permission }
            if permission == "identity.member.remove"
    ));
}

#[tokio::test]
async fn tenant_roles_cannot_contain_platform_permissions() {
    // Negative path: rejects before any transaction.
    let roles = Arc::new(MemoryRoles::new());
    let use_case = create_role(
        PgPool::connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
            .expect("lazy pool construction never connects"),
        roles.clone(),
        vec![
            "identity.member.read".to_string(),
            "platform.tenant.read".to_string(),
        ],
        Arc::new(NoopEvents),
        Arc::new(SeqIds::new()),
    );
    let err = use_case
        .execute(
            Uuid::now_v7(),
            None,
            CreateCustomRoleCommand {
                tenant_id: Uuid::now_v7(),
                key: "sneaky".to_string(),
                name: "Sneaky".to_string(),
                permissions: vec!["platform.tenant.read".to_string()],
            },
        )
        .await
        .expect_err("platform permission must be rejected in a tenant role");
    assert!(matches!(
        err,
        AccessError::PermissionScopeMismatch { permission, .. }
            if permission == "platform.tenant.read"
    ));
}

#[tokio::test]
async fn system_roles_are_immutable_on_update_and_delete() {
    let (app, _migrator, _node) = prepare().await;
    let roles = Arc::new(MemoryRoles::new());
    roles.seed_system();
    let system_role = roles
        .inner
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.is_system())
        .cloned()
        .expect("seeded");

    let tx_manager = TxManager::new(app);
    let authorizer: Arc<dyn Authorizer> = Arc::new(FixedAuthorizer {
        held: vec!["identity.member.read".to_string()],
    });
    let events = Arc::new(NoopEvents);
    let clock = Arc::new(FixedClock);
    let audit = Arc::new(NoopAudit);

    let update = UpdateCustomRole::new(
        tx_manager.clone(),
        roles.clone(),
        authorizer.clone(),
        events.clone(),
        clock.clone(),
        audit.clone(),
    );
    let err = update
        .execute(
            Uuid::now_v7(),
            None,
            UpdateCustomRoleCommand {
                role_id: system_role.id(),
                tenant_id: Uuid::now_v7(),
                name: Some("Renamed".to_string()),
                permissions: Some(vec!["identity.member.read".to_string()]),
            },
        )
        .await
        .expect_err("system roles cannot be updated");
    assert!(matches!(err, AccessError::SystemRoleImmutable { .. }));

    let delete = DeleteCustomRole::new(tx_manager, roles, events, clock, audit);
    let err = delete
        .execute(Uuid::now_v7(), None, Uuid::now_v7(), system_role.id())
        .await
        .expect_err("system roles cannot be deleted");
    assert!(matches!(err, AccessError::SystemRoleImmutable { .. }));
}

#[tokio::test]
async fn updating_a_role_rechecks_the_whole_new_set() {
    // An actor who holds identity.member.read: updating a role they created
    // to add identity.member.remove must fail even though the previous set
    // was fine.
    let (app, _migrator, _node) = prepare().await;
    let roles = Arc::new(MemoryRoles::new());
    let events = Arc::new(NoopEvents);
    let ids = Arc::new(SeqIds::new());
    let tenant = Uuid::now_v7();
    let create = create_role(
        app.clone(),
        roles.clone(),
        vec!["identity.member.read".to_string()],
        events.clone(),
        ids,
    );
    let role = create
        .execute(
            Uuid::now_v7(),
            None,
            CreateCustomRoleCommand {
                tenant_id: tenant,
                key: "agent".to_string(),
                name: "Agent".to_string(),
                permissions: vec!["identity.member.read".to_string()],
            },
        )
        .await
        .expect("create role");

    let tx_manager = TxManager::new(app);
    let update = UpdateCustomRole::new(
        tx_manager,
        roles,
        Arc::new(FixedAuthorizer {
            held: vec!["identity.member.read".to_string()],
        }),
        events,
        Arc::new(FixedClock),
        Arc::new(NoopAudit),
    );
    let err = update
        .execute(
            Uuid::now_v7(),
            None,
            UpdateCustomRoleCommand {
                role_id: role.id(),
                tenant_id: tenant,
                name: None,
                permissions: Some(vec![
                    "identity.member.read".to_string(),
                    "identity.member.remove".to_string(),
                ]),
            },
        )
        .await
        .expect_err("adding an unheld permission must fail");
    assert!(matches!(
        err,
        AccessError::Escalation { permission }
            if permission == "identity.member.remove"
    ));
}
