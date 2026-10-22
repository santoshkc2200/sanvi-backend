//! Real authoritative rows/checkpoints and advisory locks; only Keto is a
//! controlled in-memory external service, including transient failures.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use sanvi_access::{
    adapters::{
        InMemoryDecisionCache, SqlxAccessProjection, SqlxRoleRepository, SqlxTenantRoleSource,
        sync::{access_sync_handler, entitlement_sync_handler},
    },
    application::{CachedAuthorizer, CheckPermission, DegradedAuthorizer},
    domain::{
        AccessError, AccessProjection, Decision, Permission, PermissionStore, ResourceRef, Tuple,
    },
};
use sanvi_identity::adapters::repo::SqlxMembershipRepository;
use sanvi_kernel::{
    EventEnvelope, EventId, NodeId, SnowflakeGenerator, SnowflakeId, TenantId, UserId,
};
use sanvi_persistence::{PgPool, TxManager};
use sanvi_resilience::DegradationRegistry;

const TENANT: i64 = 880001;
const USER: i64 = 880002;
const EDITOR: i64 = 880003;
const VIEWER: i64 = 880004;
const OPERATOR: i64 = 880005;

#[derive(Default)]
struct Keto {
    tuples: Mutex<Vec<Tuple>>,
    fail_delete: AtomicBool,
    fail_write: AtomicBool,
    fail_checks: AtomicBool,
    pause_delete: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl Keto {
    fn contains(&self, tuple: &Tuple) -> bool {
        self.tuples.lock().unwrap().contains(tuple)
    }
}

#[async_trait]
impl PermissionStore for Keto {
    async fn write(&self, tuples: &[Tuple]) -> Result<(), AccessError> {
        if self.fail_write.swap(false, Ordering::SeqCst) {
            return Err(AccessError::BackendUnreachable(
                "temporary Keto write failure".into(),
            ));
        }
        let mut state = self.tuples.lock().unwrap();
        for tuple in tuples {
            if !state.contains(tuple) {
                state.push(tuple.clone());
            }
        }
        Ok(())
    }
    async fn delete(&self, tuples: &[Tuple]) -> Result<(), AccessError> {
        if self.fail_delete.swap(false, Ordering::SeqCst) {
            return Err(AccessError::BackendUnreachable(
                "temporary Keto failure".into(),
            ));
        }
        if self.pause_delete.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.tuples
            .lock()
            .unwrap()
            .retain(|tuple| !tuples.contains(tuple));
        Ok(())
    }
    async fn clear_namespace(&self, _: &str) -> Result<(), AccessError> {
        unreachable!("snapshot sync must not clear unrelated authorization")
    }
    async fn check_membership(
        &self,
        user: UserId,
        objects: &[String],
    ) -> Result<Vec<bool>, AccessError> {
        if self.fail_checks.load(Ordering::SeqCst) {
            return Err(AccessError::BackendUnreachable("Keto check outage".into()));
        }
        Ok(objects
            .iter()
            .map(|object| self.contains(&Tuple::role_membership(object, user)))
            .collect())
    }
}

struct Fixture {
    _db: sanvi_testsupport::TestDb,
    owner: PgPool,
    tx: TxManager,
    keto: Arc<Keto>,
}

impl Fixture {
    async fn new() -> Self {
        let db = sanvi_testsupport::test_db().await;
        let owner = db.admin_pool().await;
        sqlx::query("INSERT INTO tenancy.tenants (id, slug, display_name, status, region) VALUES ($1, 'consistency', 'Consistency', 'active', 'us')").bind(TENANT).execute(&owner).await.unwrap();
        for id in [USER, OPERATOR] {
            sqlx::query("INSERT INTO identity.users (id, kratos_identity_id, primary_email) VALUES ($1, $2, $3)")
                .bind(id).bind(uuid::Uuid::new_v4()).bind(format!("user-{id}@consistency.test")).execute(&owner).await.unwrap();
        }
        for (id, key) in [(EDITOR, "editor"), (VIEWER, "viewer")] {
            sqlx::query("INSERT INTO access.roles (id, scope, tenant_id, key, name, is_system) VALUES ($1, 'tenant', $2, $3, $3, false)").bind(id).bind(TENANT).bind(key).execute(&owner).await.unwrap();
            sqlx::query("INSERT INTO access.role_permissions (role_id, permission) VALUES ($1, 'identity.member.read')").bind(id).execute(&owner).await.unwrap();
        }
        sqlx::query("INSERT INTO access.roles (id, scope, key, name, is_system) VALUES (880006, 'platform', 'operator', 'Operator', true)").execute(&owner).await.unwrap();
        sqlx::query("INSERT INTO access.role_permissions (role_id, permission) VALUES (880006, 'identity.member.read')").execute(&owner).await.unwrap();
        sqlx::query("INSERT INTO identity.memberships (tenant_id, user_id) VALUES ($1, $2)")
            .bind(TENANT)
            .bind(USER)
            .execute(&owner)
            .await
            .unwrap();
        sqlx::query("INSERT INTO identity.membership_roles (tenant_id, user_id, role_id) VALUES ($1, $2, $3)").bind(TENANT).bind(USER).bind(EDITOR).execute(&owner).await.unwrap();
        let keto = Arc::new(Keto::default());
        keto.write(&[
            Tuple::role_membership("880001:editor", UserId::try_from(USER).unwrap()),
            Tuple::tenant_membership(
                TenantId::try_from(TENANT).unwrap(),
                UserId::try_from(USER).unwrap(),
            ),
            Tuple::role_membership("platform:operator", UserId::try_from(OPERATOR).unwrap()),
        ])
        .await
        .unwrap();
        let tx = TxManager::new(
            db.app_pool(),
            Arc::new(SnowflakeGenerator::new(NodeId::new(1).unwrap())),
        );
        Self {
            _db: db,
            owner,
            tx,
            keto,
        }
    }
    fn projection(&self) -> Arc<dyn AccessProjection> {
        Arc::new(SqlxAccessProjection::new(
            self.tx.clone(),
            Arc::new(SqlxRoleRepository),
            Arc::new(SqlxMembershipRepository),
            self.keto.clone(),
        ))
    }
    fn authorizer(&self) -> DegradedAuthorizer {
        let cache = Arc::new(InMemoryDecisionCache::new(64));
        let check = CheckPermission::new(
            self.tx.clone(),
            Arc::new(SqlxRoleRepository),
            self.keto.clone(),
            Arc::new(SqlxTenantRoleSource::new(
                self.tx.clone(),
                Arc::new(SqlxMembershipRepository),
            )),
        );
        DegradedAuthorizer::new(
            Arc::new(CachedAuthorizer::new(Arc::new(check), cache.clone(), 30)),
            cache,
            Arc::new(DegradationRegistry::default()),
        )
    }
}

fn event(kind: &str, payload: serde_json::Value) -> EventEnvelope {
    EventEnvelope::new(
        EventId::try_from(880099).unwrap(),
        "identity.membership",
        SnowflakeId::try_from(USER).unwrap(),
        kind,
        payload,
    )
}

async fn decision(authorizer: &DegradedAuthorizer, user: i64, write: bool) -> Decision {
    authorizer
        .check_with_mode(
            UserId::try_from(user).unwrap(),
            &Permission::parse("identity.member.read").unwrap(),
            &ResourceRef::Tenant(TenantId::try_from(TENANT).unwrap()),
            write,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn stopped_projection_cannot_preserve_revoked_suspended_or_changed_tenant_grants() {
    let fixture = Fixture::new().await;
    let authorizer = fixture.authorizer();
    assert_eq!(decision(&authorizer, USER, false).await, Decision::Allowed);
    sqlx::query("UPDATE identity.memberships SET status = 'suspended' WHERE user_id = $1")
        .bind(USER)
        .execute(&fixture.owner)
        .await
        .unwrap();
    for write in [false, true] {
        assert_eq!(decision(&authorizer, USER, write).await, Decision::Denied);
    }
    sqlx::query("UPDATE identity.memberships SET status = 'active' WHERE user_id = $1")
        .bind(USER)
        .execute(&fixture.owner)
        .await
        .unwrap();
    assert_eq!(decision(&authorizer, USER, false).await, Decision::Allowed);
    sqlx::query("DELETE FROM identity.membership_roles WHERE user_id = $1")
        .bind(USER)
        .execute(&fixture.owner)
        .await
        .unwrap();
    for write in [false, true] {
        assert_eq!(decision(&authorizer, USER, write).await, Decision::Denied);
    }
    sqlx::query(
        "INSERT INTO identity.membership_roles (tenant_id, user_id, role_id) VALUES ($1, $2, $3)",
    )
    .bind(TENANT)
    .bind(USER)
    .bind(EDITOR)
    .execute(&fixture.owner)
    .await
    .unwrap();
    assert_eq!(decision(&authorizer, USER, false).await, Decision::Allowed);
    sqlx::query("DELETE FROM access.role_permissions WHERE role_id = $1")
        .bind(EDITOR)
        .execute(&fixture.owner)
        .await
        .unwrap();
    for write in [false, true] {
        assert_eq!(decision(&authorizer, USER, write).await, Decision::Denied);
    }
    sqlx::query("DELETE FROM identity.memberships WHERE user_id = $1")
        .bind(USER)
        .execute(&fixture.owner)
        .await
        .unwrap();
    for write in [false, true] {
        assert_eq!(decision(&authorizer, USER, write).await, Decision::Denied);
    }
    // Elevated operators do not need an ordinary tenant membership. The
    // target of impersonation is checked using their own current identity.
    for write in [false, true] {
        assert_eq!(
            decision(&authorizer, OPERATOR, write).await,
            Decision::Allowed
        );
    }
    assert!(
        fixture.keto.contains(&Tuple::role_membership(
            "880001:editor",
            UserId::try_from(USER).unwrap()
        )),
        "the test deliberately leaves Keto stale"
    );
}

#[tokio::test]
async fn keto_outage_grace_revalidates_current_membership_roles_and_permissions() {
    for removal in ["membership", "suspension", "role", "permission"] {
        let fixture = Fixture::new().await;
        let authorizer = fixture.authorizer();
        assert_eq!(decision(&authorizer, USER, false).await, Decision::Allowed);
        assert_eq!(
            decision(&authorizer, OPERATOR, false).await,
            Decision::Allowed
        );
        fixture.keto.fail_checks.store(true, Ordering::SeqCst);
        // The existing bounded read grace remains available while current
        // authoritative grants are unchanged, including platform operators.
        assert_eq!(decision(&authorizer, USER, false).await, Decision::Allowed);
        assert_eq!(
            decision(&authorizer, OPERATOR, false).await,
            Decision::Allowed
        );
        let statement = match removal {
            "membership" => "DELETE FROM identity.memberships WHERE user_id = $1",
            "suspension" => {
                "UPDATE identity.memberships SET status = 'suspended' WHERE user_id = $1"
            }
            "role" => "DELETE FROM identity.membership_roles WHERE user_id = $1",
            "permission" => "DELETE FROM access.role_permissions WHERE role_id = $1",
            _ => unreachable!(),
        };
        sqlx::query(statement)
            .bind(if removal == "permission" {
                EDITOR
            } else {
                USER
            })
            .execute(&fixture.owner)
            .await
            .unwrap();
        assert_eq!(
            decision(&authorizer, USER, false).await,
            Decision::Denied,
            "stale read grace survived {removal}"
        );
        assert_eq!(decision(&authorizer, USER, true).await, Decision::Denied);
        assert_eq!(
            decision(&authorizer, OPERATOR, false).await,
            Decision::Allowed
        );
    }
}

#[tokio::test]
async fn failed_revocation_is_applied_when_the_same_event_is_retried() {
    let fixture = Fixture::new().await;
    sqlx::query("DELETE FROM identity.memberships WHERE user_id = $1")
        .bind(USER)
        .execute(&fixture.owner)
        .await
        .unwrap();
    fixture.keto.fail_delete.store(true, Ordering::SeqCst);
    let handler = access_sync_handler(fixture.projection(), None);
    let revoked = event(
        "identity.membership_revoked",
        serde_json::json!({"tenant_id":TENANT.to_string(), "user_id":USER.to_string(), "role_ids":[EDITOR.to_string()], "version":1}),
    );
    assert!(handler(revoked.clone()).await.is_err());
    let applied: i64 =
        sqlx::query_scalar("SELECT count(*) FROM access.membership_projection_checkpoints")
            .fetch_one(&fixture.owner)
            .await
            .unwrap();
    assert_eq!(
        applied, 0,
        "failed work must not advance the durable checkpoint"
    );
    handler(revoked).await.unwrap();
    // A late grant event cannot resurrect a membership already removed
    // from the authoritative database, even with a newer payload version.
    handler(event("identity.membership_granted", serde_json::json!({"tenant_id":TENANT.to_string(), "user_id":USER.to_string(), "role_ids":[EDITOR.to_string()], "version":999}))).await.unwrap();
    assert!(!fixture.keto.contains(&Tuple::role_membership(
        "880001:editor",
        UserId::try_from(USER).unwrap()
    )));
    assert!(!fixture.keto.contains(&Tuple::tenant_membership(
        TenantId::try_from(TENANT).unwrap(),
        UserId::try_from(USER).unwrap()
    )));
    assert!(fixture.keto.contains(&Tuple::role_membership(
        "platform:operator",
        UserId::try_from(OPERATOR).unwrap()
    )));
}

#[tokio::test]
async fn reversed_role_updates_do_not_restore_removed_permissions() {
    let fixture = Fixture::new().await;
    fixture
        .keto
        .write(&[Tuple::permission_grant(
            "identity.member.grant",
            "880001:editor",
        )])
        .await
        .unwrap();
    let handler = entitlement_sync_handler(None, None, Some(fixture.projection()));
    let newer = event(
        "access.role_updated",
        serde_json::json!({"tenant_id":TENANT.to_string(), "role_id":EDITOR.to_string(), "key":"editor", "added_permissions":[], "removed_permissions":["identity.member.grant"]}),
    );
    let older = event(
        "access.role_updated",
        serde_json::json!({"tenant_id":TENANT.to_string(), "role_id":EDITOR.to_string(), "key":"editor", "added_permissions":["identity.member.grant"], "removed_permissions":[]}),
    );
    handler(newer).await.unwrap();
    handler(older).await.unwrap();
    assert!(!fixture.keto.contains(&Tuple::permission_grant(
        "identity.member.grant",
        "880001:editor"
    )));
    assert!(fixture.keto.contains(&Tuple::permission_grant(
        "identity.member.read",
        "880001:editor"
    )));
    sqlx::query("DELETE FROM access.roles WHERE id = $1")
        .bind(EDITOR)
        .execute(&fixture.owner)
        .await
        .unwrap();
    // Delivery of a stale role-created event after deletion also converges
    // to the current absent role, rather than recreating its grants.
    handler(event("access.role_created", serde_json::json!({"tenant_id":TENANT.to_string(), "role_id":EDITOR.to_string(), "key":"editor", "permissions":["identity.member.read"]}))).await.unwrap();
    assert!(!fixture.keto.contains(&Tuple::permission_grant(
        "identity.member.read",
        "880001:editor"
    )));
}

#[tokio::test]
async fn partial_role_projection_failure_has_no_checkpoint_and_retries_the_snapshot() {
    let fixture = Fixture::new().await;
    fixture.keto.fail_write.store(true, Ordering::SeqCst);
    let handler = entitlement_sync_handler(None, None, Some(fixture.projection()));
    let update = event(
        "access.role_updated",
        serde_json::json!({"tenant_id":TENANT.to_string(), "role_id":EDITOR.to_string(), "key":"editor", "added_permissions":[], "removed_permissions":[]}),
    );
    assert!(handler(update.clone()).await.is_err());
    let applied: i64 =
        sqlx::query_scalar("SELECT count(*) FROM access.role_projection_checkpoints")
            .fetch_one(&fixture.owner)
            .await
            .unwrap();
    assert_eq!(applied, 0);
    handler(update).await.unwrap();
    assert!(fixture.keto.contains(&Tuple::permission_grant(
        "identity.member.read",
        "880001:editor"
    )));
}

#[tokio::test]
async fn membership_projection_personal_references_are_exported_and_erased() {
    use sanvi_privacy::domain::{EraseMode, PersonalDataProvider, SubjectRef};
    let fixture = Fixture::new().await;
    fixture
        .projection()
        .reconcile_membership(
            TenantId::try_from(TENANT).unwrap(),
            UserId::try_from(USER).unwrap(),
        )
        .await
        .unwrap();
    let provider = sanvi_access::adapters::privacy_provider::AccessPrivacyProvider::new(
        fixture.tx.clone(),
        Arc::new(sanvi_access::adapters::SqlxEntitlementRepository),
        fixture.keto.clone(),
    );
    let subject = SubjectRef::user(UserId::try_from(USER).unwrap());
    let bundle = provider.collect(&subject).await.unwrap();
    assert!(
        bundle
            .entries
            .iter()
            .any(|entry| entry.label == "authorization_projection"),
        "new checkpoint references belong in the subject export"
    );
    provider.erase(&subject, EraseMode::Full).await.unwrap();
    let references: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM access.membership_projection_checkpoints WHERE user_id = $1",
    )
    .bind(USER)
    .fetch_one(&fixture.owner)
    .await
    .unwrap();
    assert_eq!(references, 0);
    assert!(!fixture.keto.contains(&Tuple::role_membership(
        "880001:editor",
        UserId::try_from(USER).unwrap()
    )));
}

#[tokio::test]
async fn independent_workers_serialize_and_reload_membership_after_waiting() {
    let fixture = Fixture::new().await;
    fixture.keto.pause_delete.store(true, Ordering::SeqCst);
    let first_projection = fixture.projection();
    let first = tokio::spawn(async move {
        first_projection
            .reconcile_membership(
                TenantId::try_from(TENANT).unwrap(),
                UserId::try_from(USER).unwrap(),
            )
            .await
    });
    fixture.keto.entered.notified().await;
    // The first worker already read editor. Change the authoritative role
    // while it is in its external call; the second must load viewer after
    // acquiring the database lock, rather than replay an older role delta.
    sqlx::query("UPDATE identity.membership_roles SET role_id = $1 WHERE user_id = $2")
        .bind(VIEWER)
        .bind(USER)
        .execute(&fixture.owner)
        .await
        .unwrap();
    let second_projection = fixture.projection();
    let second = tokio::spawn(async move {
        second_projection
            .reconcile_membership(
                TenantId::try_from(TENANT).unwrap(),
                UserId::try_from(USER).unwrap(),
            )
            .await
    });
    // Observe the actual cross-worker database lock, not scheduling luck.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND NOT granted AND database = (SELECT oid FROM pg_database WHERE datname = current_database()))").fetch_one(&fixture.owner).await.unwrap();
            if waiting { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("second worker must wait on the same aggregate advisory lock");
    fixture.keto.release.notify_one();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert!(!fixture.keto.contains(&Tuple::role_membership(
        "880001:editor",
        UserId::try_from(USER).unwrap()
    )));
    assert!(fixture.keto.contains(&Tuple::role_membership(
        "880001:viewer",
        UserId::try_from(USER).unwrap()
    )));
    let applied: Vec<String> = sqlx::query_scalar("SELECT role_objects FROM access.membership_projection_checkpoints WHERE tenant_id = $1 AND user_id = $2").bind(TENANT).bind(USER).fetch_one(&fixture.owner).await.unwrap();
    assert_eq!(applied, vec!["880001:viewer"]);
}
