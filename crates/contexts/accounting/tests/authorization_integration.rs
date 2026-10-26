//! Provider consent, refresh and verification against real PostgreSQL with a
//! fake freee. Run through scripts/test.sh, never bare nextest.
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use sanvi_accounting::{
    adapters::{PostgresAccountingStore, PostgresCredentialStore},
    application::{
        AccessTokens, AccountingAccess, ActivateBook, Actor, Authorization, CompleteAuthorization,
        CreateBook, CredentialVerifier, Permission, ProviderDirectory, ProviderOAuth,
        RegisterConnection, RevokeAuthorization, Services, StartAuthorization,
    },
    domain::{
        AccountMappings, AccountingError, Book, Connection, ConnectionId, Provider,
        ProviderCatalog, Secret, TokenGrant,
    },
};
use sanvi_kernel::{Clock, TenantId, UserId};
use sanvi_persistence::TxManager;
use sanvi_testsupport::{TestDb, test_db, test_ids};

const STATE_KEY: &[u8] = b"accounting-test-state-key";

struct AllowAll;
#[async_trait]
impl AccountingAccess for AllowAll {
    async fn require(&self, _: Actor, _: Permission) -> Result<(), AccountingError> {
        Ok(())
    }
}

struct TestClock(Mutex<DateTime<Utc>>);
impl Clock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}
impl TestClock {
    fn advance(&self, by: TimeDelta) {
        *self.0.lock().unwrap() += by;
    }
}

/// Mimics freee's single-use refresh tokens: an old token is refused.
struct FakeFreee {
    clock: Arc<TestClock>,
    company: Mutex<String>,
    current_refresh: Mutex<String>,
    refreshes: AtomicUsize,
    revoked: Mutex<Vec<String>>,
    can_post: Mutex<bool>,
}

impl FakeFreee {
    fn grant(&self, n: usize) -> TokenGrant {
        TokenGrant {
            access_token: Secret::new(format!("at-{n}")),
            refresh_token: Secret::new(format!("rt-{n}")),
            access_expires_at: self.clock.now() + TimeDelta::hours(6),
            organization_id: Some(self.company.lock().unwrap().clone()),
        }
    }
}

#[async_trait]
impl ProviderOAuth for FakeFreee {
    fn provider(&self) -> Provider {
        Provider::Freee
    }
    fn authorization_url(&self, state: &str) -> String {
        format!("https://freee.test/authorize?state={state}")
    }
    async fn exchange_code(&self, code: &str) -> Result<TokenGrant, AccountingError> {
        assert_eq!(code, "good-code");
        *self.current_refresh.lock().unwrap() = "rt-0".into();
        Ok(self.grant(0))
    }
    async fn refresh(&self, refresh_token: &Secret) -> Result<TokenGrant, AccountingError> {
        // Widen the race window between concurrent callers.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut current = self.current_refresh.lock().unwrap();
        if refresh_token.expose() != *current {
            return Err(AccountingError::ReauthorizationRequired);
        }
        let n = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
        *current = format!("rt-{n}");
        Ok(self.grant(n))
    }
    async fn revoke(&self, refresh_token: &Secret) -> Result<(), AccountingError> {
        self.revoked
            .lock()
            .unwrap()
            .push(refresh_token.expose().into());
        Ok(())
    }
}

#[async_trait]
impl ProviderDirectory for FakeFreee {
    fn provider(&self) -> Provider {
        Provider::Freee
    }
    async fn catalog(&self, token: &Secret, _: &str) -> Result<ProviderCatalog, AccountingError> {
        assert!(token.expose().starts_with("at-"));
        Ok(ProviderCatalog {
            can_post: *self.can_post.lock().unwrap(),
            account_ids: ["sales", "returns", "cash"].map(String::from).into(),
            tax_codes: ["tax"].map(String::from).into(),
        })
    }
}

struct Harness {
    db: TestDb,
    services: Arc<Services>,
    authorization: Arc<Authorization>,
    tokens: Arc<AccessTokens>,
    freee: Arc<FakeFreee>,
    clock: Arc<TestClock>,
    user: Actor,
}

impl Harness {
    async fn new() -> Self {
        let db = test_db().await;
        let ids = test_ids();
        let transactions = TxManager::new(db.app_pool(), ids.clone());
        let clock = Arc::new(TestClock(Mutex::new(
            "2026-10-26T00:00:00Z".parse().unwrap(),
        )));
        let freee = Arc::new(FakeFreee {
            clock: clock.clone(),
            company: Mutex::new("42".into()),
            current_refresh: Mutex::new(String::new()),
            refreshes: AtomicUsize::new(0),
            revoked: Mutex::new(Vec::new()),
            can_post: Mutex::new(true),
        });
        let credentials = Arc::new(PostgresCredentialStore::new(
            transactions.clone(),
            ids.clone(),
            [7; 32],
            "kek-test".into(),
        ));
        let tokens = Arc::new(AccessTokens {
            credentials: credentials.clone(),
            providers: vec![freee.clone()],
            clock: clock.clone(),
        });
        let services = Arc::new(Services {
            store: Arc::new(PostgresAccountingStore::new(transactions, ids.clone())),
            access: Arc::new(AllowAll),
            verifier: Arc::new(CredentialVerifier {
                tokens: tokens.clone(),
                directories: vec![freee.clone()],
            }),
            clock: clock.clone(),
            ids,
        });
        let authorization = Arc::new(Authorization {
            services: services.clone(),
            credentials,
            providers: vec![freee.clone()],
            state_key: Arc::from(STATE_KEY),
        });
        Self {
            db,
            services,
            authorization,
            tokens,
            freee,
            clock,
            user: Actor::User {
                tenant_id: TenantId::try_from(1001).unwrap(),
                user_id: UserId::try_from(2001).unwrap(),
            },
        }
    }

    /// A book with two registered freee bindings, for companies 42 and 43.
    async fn book(&self) -> Book {
        let book = CreateBook(self.services.clone())
            .execute(self.user, "main".into())
            .await
            .unwrap();
        let book = RegisterConnection(self.services.clone())
            .execute(
                self.user,
                book.id(),
                book.revision(),
                connection(3001, "42"),
            )
            .await
            .unwrap();
        RegisterConnection(self.services.clone())
            .execute(
                self.user,
                book.id(),
                book.revision(),
                connection(3002, "43"),
            )
            .await
            .unwrap()
    }

    async fn consent(&self, book: &Book, connection: i64) -> Result<(), AccountingError> {
        let state = self.start(book, connection).await;
        CompleteAuthorization(self.authorization.clone())
            .execute(self.user, &state, "good-code")
            .await
    }

    async fn start(&self, book: &Book, connection: i64) -> String {
        let url = StartAuthorization(self.authorization.clone())
            .execute(
                self.user,
                book.id(),
                ConnectionId::try_from(connection).unwrap(),
            )
            .await
            .unwrap();
        url.split_once("state=").unwrap().1.to_string()
    }

    async fn events(&self, event_type: &str, connection: i64) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM platform.outbox WHERE aggregate_id = $1 AND event_type = $2",
        )
        .bind(connection)
        .bind(event_type)
        .fetch_one(&self.db.admin_pool().await)
        .await
        .unwrap()
    }

    async fn status(&self, connection: i64) -> Option<String> {
        sqlx::query_scalar(
            "SELECT status FROM accounting.provider_credentials WHERE connection_id = $1",
        )
        .bind(connection)
        .fetch_optional(&self.db.admin_pool().await)
        .await
        .unwrap()
    }
}

fn connection(id: i64, company: &str) -> Connection {
    Connection::new(
        ConnectionId::try_from(id).unwrap(),
        Provider::Freee,
        company.into(),
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
async fn consent_is_single_use_user_bound_and_stored_encrypted() {
    let harness = Harness::new().await;
    let book = harness.book().await;
    let state = harness.start(&book, 3001).await;

    let other_user = Actor::User {
        tenant_id: TenantId::try_from(1001).unwrap(),
        user_id: UserId::try_from(2002).unwrap(),
    };
    let complete = CompleteAuthorization(harness.authorization.clone());
    assert!(matches!(
        complete.execute(other_user, &state, "good-code").await,
        Err(AccountingError::AuthorizationRejected(_))
    ));
    assert!(matches!(
        complete
            .execute(harness.user, &format!("{state}x"), "good-code")
            .await,
        Err(AccountingError::AuthorizationRejected(_))
    ));
    complete
        .execute(harness.user, &state, "good-code")
        .await
        .unwrap();
    assert!(matches!(
        complete.execute(harness.user, &state, "good-code").await,
        Err(AccountingError::AuthorizationRejected(_))
    ));

    assert_eq!(harness.status(3001).await.as_deref(), Some("active"));
    assert_eq!(
        harness
            .events("accounting.connection_authorized", 3001)
            .await,
        1
    );
    let (refresh, access): (Vec<u8>, Vec<u8>) = sqlx::query_as(
        "SELECT refresh_ciphertext, access_ciphertext FROM accounting.provider_credentials WHERE connection_id = 3001",
    )
    .fetch_one(&harness.db.admin_pool().await)
    .await
    .unwrap();
    for ciphertext in [refresh, access] {
        assert!(!ciphertext.windows(4).any(|w| w == b"rt-0" || w == b"at-0"));
    }

    // Another tenant cannot see the row even with the app role.
    let scope_manager = TxManager::new(harness.db.app_pool(), test_ids());
    let mut other = scope_manager.begin_for(1002).await.unwrap();
    let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM accounting.provider_credentials")
        .fetch_one(&mut **other.tx_mut().as_inner())
        .await
        .unwrap();
    assert_eq!(visible, 0);
}

#[tokio::test]
async fn expired_state_is_rejected() {
    let harness = Harness::new().await;
    let book = harness.book().await;
    let state = harness.start(&book, 3001).await;
    harness.clock.advance(TimeDelta::minutes(11));
    assert!(matches!(
        CompleteAuthorization(harness.authorization.clone())
            .execute(harness.user, &state, "good-code")
            .await,
        Err(AccountingError::AuthorizationRejected(_))
    ));
    assert_eq!(harness.status(3001).await, None);
}

#[tokio::test]
async fn consent_for_another_company_is_discarded_and_revoked() {
    let harness = Harness::new().await;
    let book = harness.book().await;
    *harness.freee.company.lock().unwrap() = "99".into();
    assert!(matches!(
        harness.consent(&book, 3001).await,
        Err(AccountingError::AuthorizationRejected(_))
    ));
    assert_eq!(harness.status(3001).await, None);
    assert_eq!(*harness.freee.revoked.lock().unwrap(), vec!["rt-0"]);
}

#[tokio::test]
async fn concurrent_callers_share_one_refresh_of_a_single_use_token() {
    let harness = Harness::new().await;
    let book = harness.book().await;
    harness.consent(&book, 3001).await.unwrap();
    let connection = connection(3001, "42");
    let tenant = harness.user.tenant_id();

    // Still valid: no refresh.
    let token = harness
        .tokens
        .access_token(tenant, &connection)
        .await
        .unwrap();
    assert_eq!(token.expose(), "at-0");
    assert_eq!(harness.freee.refreshes.load(Ordering::SeqCst), 0);

    harness.clock.advance(TimeDelta::hours(7));
    let calls: Vec<_> = (0..6)
        .map(|_| {
            let tokens = harness.tokens.clone();
            let connection = connection.clone();
            tokio::spawn(async move { tokens.access_token(tenant, &connection).await })
        })
        .collect();
    for call in calls {
        assert_eq!(call.await.unwrap().unwrap().expose(), "at-1");
    }
    assert_eq!(harness.freee.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(harness.status(3001).await.as_deref(), Some("active"));
}

#[tokio::test]
async fn refused_refresh_requires_reauthorization_once() {
    let harness = Harness::new().await;
    let book = harness.book().await;
    harness.consent(&book, 3001).await.unwrap();
    // The provider no longer accepts the stored token (revoked in freee).
    *harness.freee.current_refresh.lock().unwrap() = "elsewhere".into();
    harness.clock.advance(TimeDelta::hours(7));
    let connection = connection(3001, "42");
    for _ in 0..2 {
        assert_eq!(
            harness
                .tokens
                .access_token(harness.user.tenant_id(), &connection)
                .await
                .unwrap_err(),
            AccountingError::ReauthorizationRequired
        );
    }
    assert_eq!(
        harness.status(3001).await.as_deref(),
        Some("reauthorization_required")
    );
    assert_eq!(
        harness
            .events("accounting.reauthorization_required", 3001)
            .await,
        1
    );

    // Consenting again restores the connection.
    harness.consent(&book, 3001).await.unwrap();
    assert_eq!(harness.status(3001).await.as_deref(), Some("active"));
}

#[tokio::test]
async fn activation_requires_consent_and_a_user_allowed_to_post() {
    let harness = Harness::new().await;
    let book = harness.book().await;
    let activate = ActivateBook(harness.services.clone());
    let now = harness.clock.now();
    let first = ConnectionId::try_from(3001).unwrap();
    assert_eq!(
        activate
            .execute(harness.user, book.id(), book.revision(), first, now)
            .await
            .unwrap_err(),
        AccountingError::VerificationFailed("connection has not been authorized")
    );
    harness.consent(&book, 3001).await.unwrap();
    *harness.freee.can_post.lock().unwrap() = false;
    assert!(matches!(
        activate
            .execute(harness.user, book.id(), book.revision(), first, now)
            .await,
        Err(AccountingError::VerificationFailed(_))
    ));
    *harness.freee.can_post.lock().unwrap() = true;
    let book = activate
        .execute(harness.user, book.id(), book.revision(), first, now)
        .await
        .unwrap();
    assert_eq!(book.active_connection(now), Some(first));
}

#[tokio::test]
async fn only_connections_that_no_longer_route_can_be_revoked() {
    let harness = Harness::new().await;
    let book = harness.book().await;
    harness.consent(&book, 3001).await.unwrap();
    *harness.freee.company.lock().unwrap() = "43".into();
    harness.consent(&book, 3002).await.unwrap();
    let first = ConnectionId::try_from(3001).unwrap();
    ActivateBook(harness.services.clone())
        .execute(
            harness.user,
            book.id(),
            book.revision(),
            first,
            harness.clock.now(),
        )
        .await
        .unwrap();

    let revoke = RevokeAuthorization(harness.authorization.clone());
    assert!(matches!(
        revoke.execute(harness.user, book.id(), first).await,
        Err(AccountingError::Conflict(_))
    ));
    revoke
        .execute(
            harness.user,
            book.id(),
            ConnectionId::try_from(3002).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(harness.status(3002).await.as_deref(), Some("revoked"));
    assert_eq!(
        harness.events("accounting.credential_revoked", 3002).await,
        1
    );
    let cleared: bool = sqlx::query_scalar(
        "SELECT refresh_ciphertext IS NULL AND access_ciphertext IS NULL FROM accounting.provider_credentials WHERE connection_id = 3002",
    )
    .fetch_one(&harness.db.admin_pool().await)
    .await
    .unwrap();
    assert!(cleared);
    assert_eq!(*harness.freee.revoked.lock().unwrap(), vec!["rt-0"]);
}
