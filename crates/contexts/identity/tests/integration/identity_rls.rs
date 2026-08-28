//! Integration tests: tenant isolation of the identity tables (RLS FORCE),
//! against a real Postgres with the full migration set. Proves the
//! cross-tenant escalation vector from the phase-02 plan is closed twice
//! over: structurally (ScopedTx) and by Postgres itself.

use sanvi_identity::adapters::repo::{SqlxMembershipRepository, SqlxUserRepository};
use sanvi_identity::domain::{
    Email, Membership, MembershipRepository, User, UserId, UserRepository,
};
use sanvi_kernel::{RoleId, TenantId};
use sanvi_persistence::TxManager;
use sanvi_testsupport::TestDb;

async fn prepare() -> (sanvi_persistence::PgPool, TestDb) {
    let db = sanvi_testsupport::test_db().await;
    (db.app_pool(), db)
}

async fn seed_user(tx: &TxManager, users: &SqlxUserRepository, email: &str, id: UserId) -> UserId {
    let user = User::project_new(
        id,
        uuid::Uuid::now_v7(),
        Email::parse(email).unwrap(),
        false,
        None,
        chrono::Utc::now(),
    );
    let mut scope = tx.begin_unscoped("seed", "test").await.unwrap();
    users.save(&mut scope, &user).await.unwrap();
    scope.commit().await.unwrap();
    user.id()
}

async fn grant(
    tx: &TxManager,
    memberships: &SqlxMembershipRepository,
    tenant_id: TenantId,
    user_id: UserId,
    role_ids: Vec<RoleId>,
) {
    let membership =
        Membership::grant(tenant_id, user_id, role_ids, None, chrono::Utc::now()).unwrap();
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    memberships.save(&mut scope, &membership).await.unwrap();
    scope.commit().await.unwrap();
}

#[tokio::test]
async fn memberships_are_isolated_between_tenants() {
    let (pool, _db) = prepare().await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let users = SqlxUserRepository::new();
    let memberships = SqlxMembershipRepository::new();

    let tenant_a = TenantId::try_from(101_i64).unwrap();
    let tenant_b = TenantId::try_from(102_i64).unwrap();
    for (id, slug) in [(tenant_a, "tenant-a"), (tenant_b, "tenant-b")] {
        // Runtime-checked queries, like every other test file: query
        // macros in test targets fall outside `cargo sqlx prepare`'s
        // lib-only cache and break the SQLX_OFFLINE build.
        sqlx::query(
            "INSERT INTO tenancy.tenants (id, slug, display_name, status, region)
             VALUES ($1, $2, $3, 'active', 'us')",
        )
        .bind(id.as_i64())
        .bind(slug)
        .bind(slug)
        .execute(&pool)
        .await
        .unwrap();
    }
    let alice = seed_user(
        &tx,
        &users,
        "alice@example.com",
        UserId::try_from(301_i64).unwrap(),
    )
    .await;
    let bob = seed_user(
        &tx,
        &users,
        "bob@example.com",
        UserId::try_from(302_i64).unwrap(),
    )
    .await;
    let role = RoleId::try_from(201_i64).unwrap();
    {
        // access.roles is RLS FORCE: inserts need the audited platform scope.
        let mut scope = tx.begin_unscoped("seed_role", "test").await.unwrap();
        sqlx::query(
            "INSERT INTO access.roles (id, scope, tenant_id, key, name, is_system)
             VALUES ($1, 'tenant', NULL, 'rls_test_role', 'RLS Test Role', true)",
        )
        .bind(role.as_i64())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .unwrap();
        scope.commit().await.unwrap();
    }

    // Alice is a member of A; Bob of B.
    grant(&tx, &memberships, tenant_a, alice, vec![role]).await;
    grant(&tx, &memberships, tenant_b, bob, vec![role]).await;

    // Scoped to A: alice visible, bob not.
    {
        let mut scope = tx.begin_for(tenant_a.as_i64()).await.unwrap();
        assert!(
            memberships
                .find(&mut scope, tenant_a, alice)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            memberships
                .find(&mut scope, tenant_a, bob)
                .await
                .unwrap()
                .is_none()
        );
        let listed = memberships.list(&mut scope, tenant_a).await.unwrap();
        assert_eq!(listed.len(), 1);
        scope.commit().await.unwrap();
    }

    // Scoped to B: symmetric.
    {
        let mut scope = tx.begin_for(tenant_b.as_i64()).await.unwrap();
        assert!(
            memberships
                .find(&mut scope, tenant_b, alice)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            memberships
                .find(&mut scope, tenant_b, bob)
                .await
                .unwrap()
                .is_some()
        );
        scope.commit().await.unwrap();
    }

    // Even with an explicit tenant_id parameter, RLS FORCE filters rows the
    // transaction is not scoped to — the parameter alone cannot leak.
    {
        let mut scope = tx.begin_for(tenant_a.as_i64()).await.unwrap();
        assert!(
            memberships
                .find(&mut scope, tenant_b, bob)
                .await
                .unwrap()
                .is_none()
        );
        scope.commit().await.unwrap();
    }

    // The audited platform scope sees everything (reconciliation path).
    {
        let mut scope = tx.begin_unscoped("rls_test", "test").await.unwrap();
        let all = memberships.list_all_platform(&mut scope).await.unwrap();
        assert_eq!(all.len(), 2);
        scope.commit().await.unwrap();
    }
}

#[tokio::test]
async fn raw_sql_cannot_bypass_rls_either() {
    let (pool, _db) = prepare().await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let mut scope = tx.begin_for(TenantId::try_from(103_i64).unwrap().as_i64()).await.unwrap();
    // FORCE RLS: a bare SELECT from the NOBYPASSRLS role sees nothing for
    // this tenant (the WHERE clause alone cannot override the policy).
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM identity.memberships WHERE tenant_id = $1")
            .bind(TenantId::try_from(104_i64).unwrap().as_i64())
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .unwrap();
    assert_eq!(count, 0);
    scope.commit().await.unwrap();
}
