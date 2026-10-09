use sanvi_access::{
    adapters::{HttpKetoClient, InMemoryDecisionCache, SqlxRoleRepository},
    application::{CachedAuthorizer, CheckPermission, DegradedAuthorizer},
    domain::{AccessError, Authorizer, Decision, DecisionCache, Permission, ResourceRef},
};
use sanvi_kernel::{SnowflakeGenerator, TenantId, UserId};
use sanvi_persistence::TxManager;
use sanvi_resilience::{DegradationReason, DegradationRegistry};
use std::sync::Arc;

#[tokio::test]
async fn production_cache_cannot_bypass_a_fresh_check_or_fabricate_keto_recovery() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://test:test@127.0.0.1:1/test")
        .unwrap();
    pool.close().await;
    let check = Arc::new(CheckPermission::new(
        TxManager::new(
            pool,
            Arc::new(SnowflakeGenerator::new(
                sanvi_kernel::NodeId::new(1).unwrap(),
            )),
        ),
        Arc::new(SqlxRoleRepository),
        Arc::new(
            HttpKetoClient::new("http://127.0.0.1:1".into(), "http://127.0.0.1:1".into()).unwrap(),
        ),
    ));
    let cache: Arc<dyn DecisionCache> = Arc::new(InMemoryDecisionCache::new(32));
    let user = UserId::try_from(42_i64).unwrap();
    let resource = ResourceRef::Tenant(TenantId::try_from(101_i64).unwrap());
    let permission = Permission::parse("identity.member.grant").unwrap();
    cache
        .put("42:identity.member.grant:tenant:101", Decision::Allowed, 30)
        .await;
    let cached = Arc::new(CachedAuthorizer::new(check, cache.clone(), 30));
    assert_eq!(
        cached.check(user, &permission, &resource).await.unwrap(),
        Decision::Allowed
    );
    assert!(
        matches!(
            cached.check_fresh(user, &permission, &resource).await,
            Err(AccessError::Storage(_))
        ),
        "fresh check must reach the closed pool despite cached allow"
    );

    let registry = Arc::new(DegradationRegistry::default());
    registry.set(DegradationReason::Keto, true);
    let degraded = DegradedAuthorizer::new(cached, cache, registry.clone());
    let own_profile = Permission::parse("identity.profile.read").unwrap();
    assert_eq!(
        degraded
            .check_with_mode(user, &own_profile, &ResourceRef::User(user), false)
            .await
            .unwrap(),
        Decision::Allowed
    );
    assert!(
        registry.is_active(DegradationReason::Keto),
        "an authenticated-only decision never checked Keto"
    );
    assert_eq!(
        degraded
            .check_with_mode(user, &own_profile, &ResourceRef::User(user), true)
            .await
            .unwrap(),
        Decision::Denied
    );
}
