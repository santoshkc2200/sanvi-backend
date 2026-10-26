//! Real PostgreSQL tests. Run through scripts/test.sh, never bare nextest.
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use sanvi_accounting::{
    adapters::{PostgresAccountingStore, UnavailableVerifier},
    application::{
        AccountingAccess, ActivateBook, Actor, AssignCommand, AssignPosting, CreateBook, GetBook,
        MarkSubmitted, Permission, ProviderVerifier, RecordDelivery, RecordFailure,
        RegisterConnection, ResolvePosting, ScheduleSwitch, Services,
    },
    domain::{
        AccountMappings, AccountingError, Book, Connection, ConnectionId, PostingStatus, Provider,
    },
};
use sanvi_kernel::{Clock, EventId, TenantId, UserId};
use sanvi_persistence::TxManager;
use sanvi_testsupport::{TestDb, test_db, test_ids};

struct TestAccess(bool);
#[async_trait]
impl AccountingAccess for TestAccess {
    async fn require(&self, _: Actor, _: Permission) -> Result<(), AccountingError> {
        if self.0 {
            Ok(())
        } else {
            Err(AccountingError::Forbidden)
        }
    }
}

/// The only always-successful provider verifier is confined to this test binary.
struct TestVerifier;
#[async_trait]
impl ProviderVerifier for TestVerifier {
    async fn verify(&self, _: TenantId, connection: &Connection) -> Result<(), AccountingError> {
        connection.mappings().validate()
    }
}

struct TestClock(Mutex<DateTime<Utc>>);
impl Clock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}
impl TestClock {
    fn set(&self, now: DateTime<Utc>) {
        *self.0.lock().unwrap() = now;
    }
}

struct Harness {
    db: TestDb,
    services: Arc<Services>,
    clock: Arc<TestClock>,
    user: Actor,
    system: Actor,
}
impl Harness {
    async fn new() -> Self {
        let db = test_db().await;
        let ids = test_ids();
        let transactions = TxManager::new(db.app_pool(), ids.clone());
        let clock = Arc::new(TestClock(Mutex::new(
            "2026-10-26T00:00:00Z".parse().unwrap(),
        )));
        let services = Arc::new(Services {
            store: Arc::new(PostgresAccountingStore::new(transactions, ids.clone())),
            access: Arc::new(TestAccess(true)),
            verifier: Arc::new(TestVerifier),
            clock: clock.clone(),
            ids,
        });
        let user = Actor::User {
            tenant_id: TenantId::try_from(1001).unwrap(),
            user_id: UserId::try_from(2001).unwrap(),
        };
        let system = Actor::System {
            tenant_id: TenantId::try_from(1001).unwrap(),
            service: "accounting-test",
        };
        Self {
            db,
            services,
            clock,
            user,
            system,
        }
    }

    async fn configured(&self) -> Book {
        let services = &self.services;
        let book = CreateBook(services.clone())
            .execute(self.user, "main".into())
            .await
            .unwrap();
        let first = connection(3001, Provider::Freee);
        let first_id = first.id();
        let book = RegisterConnection(services.clone())
            .execute(self.user, book.id(), book.revision(), first)
            .await
            .unwrap();
        let book = RegisterConnection(services.clone())
            .execute(
                self.user,
                book.id(),
                book.revision(),
                connection(3002, Provider::Xero),
            )
            .await
            .unwrap();
        ActivateBook(services.clone())
            .execute(
                self.user,
                book.id(),
                book.revision(),
                first_id,
                self.clock.now(),
            )
            .await
            .unwrap()
    }

    fn command(&self, book: &Book, event_id: i64) -> AssignCommand {
        AssignCommand {
            book_id: book.id(),
            source_event_id: EventId::try_from(event_id).unwrap(),
            fingerprint: "a".repeat(64),
            accounting_at: self.clock.now(),
        }
    }
}

fn connection(id: i64, provider: Provider) -> Connection {
    Connection::new(
        ConnectionId::try_from(id).unwrap(),
        provider,
        format!("company-{id}"),
        AccountMappings {
            revenue_account: "sales".into(),
            refund_account: "returns".into(),
            clearing_account: "cash".into(),
            tax_codes: std::collections::BTreeMap::from([("standard".into(), "tax".into())]),
        },
    )
    .unwrap()
}

#[tokio::test]
async fn export_is_system_only_and_management_is_user_only() {
    let harness = Harness::new().await;
    let book = harness.configured().await;
    let cmd = harness.command(&book, 4001);

    assert_eq!(
        AssignPosting(harness.services.clone())
            .execute(harness.user, cmd.clone())
            .await
            .unwrap_err(),
        AccountingError::Forbidden
    );
    assert_eq!(
        CreateBook(harness.services.clone())
            .execute(harness.system, "forbidden".into())
            .await
            .unwrap_err(),
        AccountingError::Forbidden
    );
    assert_eq!(
        GetBook(harness.services.clone())
            .execute(harness.system, book.id())
            .await
            .unwrap_err(),
        AccountingError::Forbidden
    );
    assert!(
        AssignPosting(harness.services.clone())
            .execute(harness.system, cmd)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn late_event_after_cutover_routes_to_previous_binding() {
    let harness = Harness::new().await;
    let service = &harness.services;
    let user = harness.user;
    let system = harness.system;
    let book = harness.configured().await;
    let now = harness.clock.now();
    let original = harness.command(&book, 4001);
    let posting = AssignPosting(service.clone())
        .execute(system, original.clone())
        .await
        .unwrap();
    let cutoff = now + TimeDelta::hours(1);
    let book = ScheduleSwitch(service.clone())
        .execute(
            user,
            book.id(),
            book.revision(),
            ConnectionId::try_from(3002).unwrap(),
            cutoff,
        )
        .await
        .unwrap();
    harness.clock.set(now + TimeDelta::hours(2));

    let mut cmd_4002 = harness.command(&book, 4002);
    cmd_4002.accounting_at = now + TimeDelta::minutes(30);
    let post_4002 = AssignPosting(service.clone())
        .execute(system, cmd_4002)
        .await
        .unwrap();
    assert_eq!(
        post_4002.connection_id,
        ConnectionId::try_from(3001).unwrap()
    );

    let mut cmd_4003 = harness.command(&book, 4003);
    cmd_4003.accounting_at = now + TimeDelta::minutes(90);
    let post_4003 = AssignPosting(service.clone())
        .execute(system, cmd_4003)
        .await
        .unwrap();
    assert_eq!(
        post_4003.connection_id,
        ConnectionId::try_from(3002).unwrap()
    );

    let replay = AssignPosting(service.clone())
        .execute(system, original.clone())
        .await
        .unwrap();
    assert_eq!(replay.id, posting.id);
    assert_eq!(replay.connection_id, ConnectionId::try_from(3001).unwrap());

    let mut changed = original;
    changed.fingerprint = "b".repeat(64);
    assert_eq!(
        AssignPosting(service.clone())
            .execute(system, changed)
            .await
            .unwrap_err(),
        AccountingError::IdempotencyConflict
    );
}

#[tokio::test]
async fn tenant_rls_and_column_permissions_protect_history() {
    let harness = Harness::new().await;
    let book = harness.configured().await;
    let other_user = Actor::User {
        tenant_id: TenantId::try_from(1002).unwrap(),
        user_id: UserId::try_from(2002).unwrap(),
    };
    let other_system = Actor::System {
        tenant_id: TenantId::try_from(1002).unwrap(),
        service: "other-test",
    };
    assert_eq!(
        GetBook(harness.services.clone())
            .execute(other_user, book.id())
            .await
            .unwrap_err(),
        AccountingError::NotFound
    );
    let posting = AssignPosting(harness.services.clone())
        .execute(harness.system, harness.command(&book, 4001))
        .await
        .unwrap();
    assert_eq!(
        RecordFailure(harness.services.clone())
            .execute(other_system, posting.id, "not-owned")
            .await
            .unwrap_err(),
        AccountingError::NotFound
    );
    let pool = harness.db.app_pool();
    let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM accounting.books")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(visible, 0);
    let manager = TxManager::new(pool, test_ids());
    let mut scope = manager
        .begin_for(harness.user.tenant_id().as_i64())
        .await
        .unwrap();
    let error = sqlx::query("UPDATE accounting.postings SET connection_id = 9999 WHERE id = $1")
        .bind(posting.id.as_i64())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("42501")
    );
}

#[tokio::test]
async fn concurrent_management_changes_have_one_winner() {
    let harness = Harness::new().await;
    let book = harness.configured().await;
    let first = RegisterConnection(harness.services.clone());
    let second = RegisterConnection(harness.services.clone());
    let (one, two) = tokio::join!(
        first.execute(
            harness.user,
            book.id(),
            book.revision(),
            connection(3003, Provider::MoneyForward)
        ),
        second.execute(
            harness.user,
            book.id(),
            book.revision(),
            connection(3004, Provider::MoneyForward)
        )
    );
    assert_eq!(usize::from(one.is_ok()) + usize::from(two.is_ok()), 1);
    assert!(
        matches!(one, Err(AccountingError::StaleRevision))
            || matches!(two, Err(AccountingError::StaleRevision))
    );
}

#[tokio::test]
async fn concurrent_source_replays_create_one_assignment_and_one_event() {
    let harness = Harness::new().await;
    let book = harness.configured().await;
    harness
        .clock
        .set(harness.clock.now() + TimeDelta::seconds(1) + TimeDelta::nanoseconds(123));
    let first = AssignPosting(harness.services.clone());
    let second = AssignPosting(harness.services.clone());
    let (one, two) = tokio::join!(
        first.execute(harness.system, harness.command(&book, 4001)),
        second.execute(harness.system, harness.command(&book, 4001))
    );
    assert_eq!(one.unwrap().id, two.unwrap().id);
    let pool = harness.db.admin_pool().await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM accounting.postings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.outbox WHERE event_type = 'accounting.posting_assigned'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn provider_verification_and_authorization_fail_closed() {
    let harness = Harness::new().await;
    let book = CreateBook(harness.services.clone())
        .execute(harness.user, "main".into())
        .await
        .unwrap();
    let book = RegisterConnection(harness.services.clone())
        .execute(
            harness.user,
            book.id(),
            book.revision(),
            connection(3001, Provider::Freee),
        )
        .await
        .unwrap();
    let restricted = Arc::new(Services {
        store: harness.services.store.clone(),
        access: Arc::new(TestAccess(true)),
        verifier: Arc::new(UnavailableVerifier),
        clock: harness.clock.clone(),
        ids: test_ids(),
    });
    assert_eq!(
        ActivateBook(restricted.clone())
            .execute(
                harness.user,
                book.id(),
                book.revision(),
                ConnectionId::try_from(3001).unwrap(),
                harness.clock.now()
            )
            .await
            .unwrap_err(),
        AccountingError::VerificationUnavailable
    );
    let unchanged = GetBook(restricted)
        .execute(harness.user, book.id())
        .await
        .unwrap();
    assert!(unchanged.active_connection(harness.clock.now()).is_none());
    let denied = Arc::new(Services {
        store: harness.services.store.clone(),
        access: Arc::new(TestAccess(false)),
        verifier: Arc::new(TestVerifier),
        clock: harness.clock.clone(),
        ids: test_ids(),
    });
    assert_eq!(
        CreateBook(denied)
            .execute(harness.user, "forbidden".into())
            .await
            .unwrap_err(),
        AccountingError::Forbidden
    );
}

#[tokio::test]
async fn outbox_failure_rolls_back_book_and_posting_mutations() {
    let harness = Harness::new().await;
    let book = harness.configured().await;
    let admin = harness.db.admin_pool().await;
    sqlx::query("CREATE FUNCTION platform.reject_accounting_events() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.aggregate = 'accounting.book' THEN RAISE EXCEPTION 'injected accounting outbox failure'; END IF; RETURN NEW; END; $$")
        .execute(&admin).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_accounting_events BEFORE INSERT ON platform.outbox FOR EACH ROW EXECUTE FUNCTION platform.reject_accounting_events()")
        .execute(&admin).await.unwrap();
    assert!(
        RegisterConnection(harness.services.clone())
            .execute(
                harness.user,
                book.id(),
                book.revision(),
                connection(3003, Provider::MoneyForward)
            )
            .await
            .is_err()
    );
    assert!(
        AssignPosting(harness.services.clone())
            .execute(harness.system, harness.command(&book, 4001))
            .await
            .is_err()
    );
    let unchanged = GetBook(harness.services.clone())
        .execute(harness.user, book.id())
        .await
        .unwrap();
    assert_eq!(unchanged, book);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM accounting.postings")
        .fetch_one(&admin)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn duplicate_legal_entity_and_conflicting_delivery_are_rejected() {
    let harness = Harness::new().await;
    let book = harness.configured().await;
    assert!(matches!(
        CreateBook(harness.services.clone())
            .execute(harness.user, "main".into())
            .await,
        Err(AccountingError::Conflict(_))
    ));
    let posting = AssignPosting(harness.services.clone())
        .execute(harness.system, harness.command(&book, 4001))
        .await
        .unwrap();
    MarkSubmitted(harness.services.clone())
        .execute(harness.system, posting.id)
        .await
        .unwrap();
    let recorder = RecordDelivery(harness.services.clone());
    let original = recorder
        .execute(harness.system, posting.id, "external-1")
        .await
        .unwrap();
    assert_eq!(
        recorder
            .execute(harness.system, posting.id, "external-1")
            .await
            .unwrap(),
        original
    );
    assert!(matches!(
        recorder
            .execute(harness.system, posting.id, "external-2")
            .await,
        Err(AccountingError::Conflict(_))
    ));
    let manager = TxManager::new(harness.db.app_pool(), test_ids());
    let mut scope = manager
        .begin_for(harness.user.tenant_id().as_i64())
        .await
        .unwrap();
    let error = sqlx::query("UPDATE accounting.postings SET status = 'pending' WHERE id = $1")
        .bind(posting.id.as_i64())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23514")
    );
}

#[tokio::test]
async fn posting_lifecycle_writes_outbox_events() {
    let harness = Harness::new().await;
    let book = harness.configured().await;
    let post = AssignPosting(harness.services.clone())
        .execute(harness.system, harness.command(&book, 4001))
        .await
        .unwrap();
    MarkSubmitted(harness.services.clone())
        .execute(harness.system, post.id)
        .await
        .unwrap();
    RecordFailure(harness.services.clone())
        .execute(harness.system, post.id, "provider timeout")
        .await
        .unwrap();
    MarkSubmitted(harness.services.clone())
        .execute(harness.system, post.id)
        .await
        .unwrap();
    // idempotent repeat adds no event
    MarkSubmitted(harness.services.clone())
        .execute(harness.system, post.id)
        .await
        .unwrap();
    RecordDelivery(harness.services.clone())
        .execute(harness.system, post.id, "ext-invoice-1")
        .await
        .unwrap();
    // idempotent repeat adds no event
    RecordDelivery(harness.services.clone())
        .execute(harness.system, post.id, "ext-invoice-1")
        .await
        .unwrap();

    let admin = harness.db.admin_pool().await;
    let count_assigned: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.outbox WHERE aggregate_id = $1 AND event_type = 'accounting.posting_assigned'",
    )
    .bind(book.id().as_i64())
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(count_assigned, 1);

    let count_submitted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.outbox WHERE aggregate_id = $1 AND event_type = 'accounting.posting_submitted'",
    )
    .bind(book.id().as_i64())
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(count_submitted, 2);

    let count_failed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.outbox WHERE aggregate_id = $1 AND event_type = 'accounting.posting_failed'",
    )
    .bind(book.id().as_i64())
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(count_failed, 1);

    let count_delivered: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.outbox WHERE aggregate_id = $1 AND event_type = 'accounting.posting_delivered'",
    )
    .bind(book.id().as_i64())
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(count_delivered, 1);
}

#[tokio::test]
async fn user_can_resolve_stuck_posting() {
    let harness = Harness::new().await;
    let book = harness.configured().await;
    let post = AssignPosting(harness.services.clone())
        .execute(harness.system, harness.command(&book, 4001))
        .await
        .unwrap();
    MarkSubmitted(harness.services.clone())
        .execute(harness.system, post.id)
        .await
        .unwrap();

    let resolved = ResolvePosting(harness.services.clone())
        .execute(
            harness.user,
            post.id,
            "entered by hand in freee",
            Some("FREEE-123"),
        )
        .await
        .unwrap();

    assert_eq!(
        resolved.status,
        PostingStatus::ResolvedManually {
            note: "entered by hand in freee".into(),
            external_record_id: Some("FREEE-123".into()),
            resolved_by: UserId::try_from(2001).unwrap(),
        }
    );

    assert_eq!(
        ResolvePosting(harness.services.clone())
            .execute(harness.system, post.id, "note", None)
            .await
            .unwrap_err(),
        AccountingError::Forbidden
    );

    assert_eq!(
        MarkSubmitted(harness.services.clone())
            .execute(harness.user, post.id)
            .await
            .unwrap_err(),
        AccountingError::Forbidden
    );
}

#[tokio::test]
async fn concurrent_assignments_at_cutover_route_by_date() {
    let harness = Harness::new().await;
    let book = harness.configured().await;
    let cutoff = harness.clock.now() + TimeDelta::hours(1);
    let book = ScheduleSwitch(harness.services.clone())
        .execute(
            harness.user,
            book.id(),
            book.revision(),
            ConnectionId::try_from(3002).unwrap(),
            cutoff,
        )
        .await
        .unwrap();
    harness.clock.set(cutoff);

    let mut cmd1 = harness.command(&book, 4001);
    cmd1.accounting_at = cutoff - TimeDelta::microseconds(1);
    let mut cmd2 = harness.command(&book, 4002);
    cmd2.accounting_at = cutoff;

    let assign1 = AssignPosting(harness.services.clone());
    let assign2 = AssignPosting(harness.services.clone());
    let (res1, res2) = tokio::join!(
        assign1.execute(harness.system, cmd1),
        assign2.execute(harness.system, cmd2)
    );
    assert_eq!(
        res1.unwrap().connection_id,
        ConnectionId::try_from(3001).unwrap()
    );
    assert_eq!(
        res2.unwrap().connection_id,
        ConnectionId::try_from(3002).unwrap()
    );
}

#[tokio::test]
async fn accounting_migration_reverses_without_touching_other_contexts() {
    let db = test_db().await;
    let admin = db.admin_pool().await;
    for down in [
        include_str!(
            "../../../../migrations/20261026000300_accounting_provider_credentials.down.sql"
        ),
        include_str!("../../../../migrations/20261026000200_accounting_destinations.down.sql"),
    ] {
        sqlx::raw_sql(down).execute(&admin).await.unwrap();
    }
    let absent: bool = sqlx::query_scalar("SELECT to_regnamespace('accounting') IS NULL AND to_regclass('tenancy.tenants') IS NOT NULL").fetch_one(&admin).await.unwrap();
    assert!(absent);
    for up in [
        include_str!("../../../../migrations/20261026000200_accounting_destinations.sql"),
        include_str!("../../../../migrations/20261026000300_accounting_provider_credentials.sql"),
    ] {
        sqlx::raw_sql(up).execute(&admin).await.unwrap();
    }
    let forced: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_class WHERE relnamespace = 'accounting'::regnamespace AND relname IN ('books', 'postings', 'oauth_states', 'tenant_secret_keys', 'provider_credentials') AND relrowsecurity AND relforcerowsecurity").fetch_one(&admin).await.unwrap();
    assert_eq!(forced, 5);
}

#[tokio::test]
async fn privacy_metadata_requires_review_without_exposing_or_erasing_other_tenants() {
    use sanvi_accounting::adapters::privacy_provider::AccountingPrivacyProvider;
    use sanvi_privacy::domain::{EraseMode, PersonalDataProvider, PrivacyError, SubjectRef};

    let harness = Harness::new().await;
    let book = harness.configured().await;
    let provider =
        AccountingPrivacyProvider::new(TxManager::new(harness.db.app_pool(), test_ids()));
    let mut owner = SubjectRef::user(UserId::try_from(2001).unwrap());
    owner.tenant_id = Some(TenantId::try_from(1001).unwrap());
    assert!(matches!(
        provider.collect(&owner).await,
        Err(PrivacyError::ProcessingPending(_))
    ));
    assert!(matches!(
        provider.erase(&owner, EraseMode::Full).await,
        Err(PrivacyError::ProcessingPending(_))
    ));
    let mut other = owner.clone();
    other.tenant_id = Some(TenantId::try_from(1002).unwrap());
    assert!(provider.collect(&other).await.unwrap().entries.is_empty());
    assert_eq!(
        provider
            .erase(&other, EraseMode::Full)
            .await
            .unwrap()
            .erased,
        0
    );
    let customer = SubjectRef::end_user(TenantId::try_from(1001).unwrap(), "customer-1");
    assert!(
        provider
            .collect(&customer)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    let unchanged = GetBook(harness.services.clone())
        .execute(harness.user, book.id())
        .await
        .unwrap();
    assert_eq!(unchanged, book);
}
