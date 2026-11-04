use sanvi_kernel::TenantId;
use sanvi_persistence::IdempotencyStore;
use sanvi_product_access::adapters::StoredCheckoutReservations;
use sanvi_product_access::domain::{CheckoutAllocation, CheckoutReservations};

#[tokio::test]
async fn allocation_survives_adapter_recreation_and_is_scoped_and_first_writer_wins() {
    let db = sanvi_testsupport::test_db().await;
    let store = || StoredCheckoutReservations::new(IdempotencyStore::new(db.app_pool()));
    let tenant = TenantId::try_from(1001_i64).unwrap();
    let other = TenantId::try_from(1002_i64).unwrap();
    let original = vec![CheckoutAllocation {
        variant_id: "900".into(),
        location_id: "5".into(),
        quantity: 2,
    }];
    let changed = vec![CheckoutAllocation {
        location_id: "6".into(),
        ..original[0].clone()
    }];
    assert_eq!(store().load(tenant, 42, "intent").await.unwrap(), None);
    let first = store();
    let second = store();
    let (a, b) = tokio::join!(
        first.remember(tenant, 42, "intent", original.clone()),
        second.remember(tenant, 42, "intent", changed.clone())
    );
    // A concurrent unfinished claim can ask its caller to retry; every
    // successful writer must see the same allocation after it resolves.
    let winner = store().load(tenant, 42, "intent").await.unwrap().unwrap();
    assert!(winner == original || winner == changed);
    for result in [a, b].into_iter().flatten() {
        assert_eq!(result, winner);
    }
    assert_eq!(
        store()
            .remember(tenant, 42, "intent", changed)
            .await
            .unwrap(),
        winner
    );
    assert_eq!(store().load(other, 42, "intent").await.unwrap(), None);
    assert_eq!(store().load(tenant, 43, "intent").await.unwrap(), None);
}
