//! Composition root wiring for the accounting bounded context.

use std::sync::Arc;

use async_trait::async_trait;
use sanvi_access::domain::{Authorizer, Decision, Permission as AccessPermission, ResourceRef};
use sanvi_accounting::{
    AccountingConfig,
    adapters::{
        FreeeClient, FreeeSettings, PostgresAccountingStore, PostgresCredentialStore,
        UnavailableVerifier,
        http::{AccountingState, Consent},
    },
    application::{
        AccessTokens, AccountingAccess, AccountingStore, ActivateBook, Actor, Authorization,
        CancelSwitch, CompleteAuthorization, CreateBook, CredentialVerifier, GetBook, ListBooks,
        Permission as AccountingPermission, ProviderVerifier, RegisterConnection,
        RevokeAuthorization, ScheduleSwitch, Services, StartAuthorization,
    },
    domain::{AccountingError, Secret},
};
use sanvi_kernel::{Clock, RawIdGenerator};
use sanvi_persistence::TxManager;

pub struct AuthorizerAccess(pub Arc<dyn Authorizer>);

#[async_trait]
impl AccountingAccess for AuthorizerAccess {
    async fn require(
        &self,
        actor: Actor,
        permission: AccountingPermission,
    ) -> Result<(), AccountingError> {
        let (tenant_id, user_id) = match actor {
            Actor::User { tenant_id, user_id } => (tenant_id, user_id),
            Actor::System { .. } => return Err(AccountingError::Forbidden),
        };
        let (key, fresh) = match permission {
            AccountingPermission::Read => ("tenant.accounting.read", false),
            AccountingPermission::Manage => ("tenant.accounting.manage", true),
            AccountingPermission::Export => return Err(AccountingError::Forbidden),
        };
        let perm =
            AccessPermission::parse(key).map_err(|e| AccountingError::Storage(e.to_string()))?;
        let resource = ResourceRef::Tenant(tenant_id);
        let decision = if fresh {
            self.0
                .check_fresh(user_id, &perm, &resource)
                .await
                .map(|(d, _)| d)
        } else {
            self.0.check(user_id, &perm, &resource).await
        };
        match decision {
            Ok(Decision::Allowed) => Ok(()),
            Ok(Decision::Denied) => Err(AccountingError::Forbidden),
            Err(e) => {
                tracing::warn!(error = %e, "authorizer check failed for accounting");
                Err(AccountingError::ProviderUnavailable)
            }
        }
    }
}

pub fn build_accounting_state(
    config: &AccountingConfig,
    transactions: TxManager,
    ids: Arc<dyn RawIdGenerator>,
    authorizer: Arc<dyn Authorizer>,
    clock: Arc<dyn Clock>,
) -> anyhow::Result<AccountingState> {
    let store = Arc::new(PostgresAccountingStore::new(
        transactions.clone(),
        ids.clone(),
    ));
    build_accounting_state_with_store(config, store, transactions, ids, authorizer, clock)
}

pub fn build_accounting_state_with_store(
    config: &AccountingConfig,
    store: Arc<dyn AccountingStore>,
    transactions: TxManager,
    ids: Arc<dyn RawIdGenerator>,
    authorizer: Arc<dyn Authorizer>,
    clock: Arc<dyn Clock>,
) -> anyhow::Result<AccountingState> {
    let access = Arc::new(AuthorizerAccess(authorizer));

    let (verifier, consent): (Arc<dyn ProviderVerifier>, Option<Consent>) =
        if config.secrets_present() {
            if config.freee_redirect_uri.trim().is_empty() {
                anyhow::bail!("accounting.freee_redirect_uri must not be empty");
            }
            let kek = config.token_kek_bytes()?;
            if config.token_kek_id.trim().is_empty() {
                anyhow::bail!("accounting.token_kek_id must not be empty");
            }

            let freee_settings = FreeeSettings::production(
                config.freee_client_id.clone(),
                Secret::new(config.freee_client_secret.expose_secret().clone()),
                config.freee_redirect_uri.clone(),
            );
            let freee_client = Arc::new(
                FreeeClient::new(freee_settings)
                    .map_err(|e| anyhow::anyhow!("failed to initialize freee client: {e}"))?,
            );

            let credential_store = Arc::new(PostgresCredentialStore::new(
                transactions,
                ids.clone(),
                kek,
                config.token_kek_id.clone(),
            ));

            let tokens = Arc::new(AccessTokens {
                credentials: credential_store.clone(),
                providers: vec![freee_client.clone()],
                clock: clock.clone(),
            });

            let verifier: Arc<dyn ProviderVerifier> = Arc::new(CredentialVerifier {
                tokens,
                directories: vec![freee_client.clone()],
            });

            let state_key: Arc<[u8]> =
                Arc::from(config.oauth_state_signing_secret.expose_secret().as_bytes());

            let services = Arc::new(Services {
                store: store.clone(),
                access: access.clone(),
                verifier: verifier.clone(),
                clock: clock.clone(),
                ids: ids.clone(),
            });

            let authorization = Arc::new(Authorization {
                services,
                credentials: credential_store,
                providers: vec![freee_client],
                state_key,
            });

            let consent = Some(Consent {
                start: Arc::new(StartAuthorization(authorization.clone())),
                complete: Arc::new(CompleteAuthorization(authorization.clone())),
                revoke: Arc::new(RevokeAuthorization(authorization)),
            });

            (verifier, consent)
        } else {
            let missing = config.missing_secret_keys();
            tracing::warn!(
                missing = ?missing,
                "accounting: freee secrets missing; consent disabled and verification fails closed"
            );
            (Arc::new(UnavailableVerifier), None)
        };

    let services = Arc::new(Services {
        store,
        access,
        verifier,
        clock,
        ids,
    });

    Ok(AccountingState {
        list_books: Arc::new(ListBooks(services.clone())),
        get_book: Arc::new(GetBook(services.clone())),
        create_book: Arc::new(CreateBook(services.clone())),
        register_connection: Arc::new(RegisterConnection(services.clone())),
        activate_book: Arc::new(ActivateBook(services.clone())),
        schedule_switch: Arc::new(ScheduleSwitch(services.clone())),
        cancel_switch: Arc::new(CancelSwitch(services)),
        consent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use sanvi_access::domain::AccessError;
    use sanvi_accounting::{
        application::{
            AssignCommand, AuditRecord, RequestContext,
            ports::{BookChange, PostingChange},
        },
        domain::{
            AccountMappings, AccountingEvent, Book, BookId, Connection, ConnectionId,
            CredentialStatus, Posting, PostingId, Provider,
        },
    };
    use sanvi_kernel::{TenantId, UserId};
    use sanvi_telemetry::Redacted;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    struct StubAuthorizer {
        recorded: Mutex<Vec<(String, bool)>>,
        error_on_check: bool,
    }

    #[async_trait]
    impl Authorizer for StubAuthorizer {
        async fn check(
            &self,
            _user_id: UserId,
            permission: &AccessPermission,
            _resource: &ResourceRef,
        ) -> Result<Decision, AccessError> {
            if self.error_on_check {
                return Err(AccessError::BackendUnreachable("down".into()));
            }
            self.recorded
                .lock()
                .unwrap()
                .push((permission.as_str().to_string(), false));
            Ok(Decision::Allowed)
        }

        async fn check_fresh(
            &self,
            _user_id: UserId,
            permission: &AccessPermission,
            _resource: &ResourceRef,
        ) -> Result<(Decision, bool), AccessError> {
            if self.error_on_check {
                return Err(AccessError::BackendUnreachable("down".into()));
            }
            self.recorded
                .lock()
                .unwrap()
                .push((permission.as_str().to_string(), true));
            Ok((Decision::Allowed, true))
        }
    }

    struct StubStore {
        book: Mutex<Book>,
    }

    #[async_trait]
    impl AccountingStore for StubStore {
        async fn create(
            &self,
            _book: Book,
            _event: AccountingEvent,
            _audit: AuditRecord,
        ) -> Result<Book, AccountingError> {
            unimplemented!()
        }
        async fn get(
            &self,
            _tenant_id: TenantId,
            _book_id: BookId,
        ) -> Result<Book, AccountingError> {
            Ok(self.book.lock().unwrap().clone())
        }
        async fn list(&self, _tenant_id: TenantId) -> Result<Vec<Book>, AccountingError> {
            Ok(vec![self.book.lock().unwrap().clone()])
        }
        async fn credential_statuses(
            &self,
            _tenant_id: TenantId,
            _book_id: BookId,
        ) -> Result<Vec<(ConnectionId, CredentialStatus)>, AccountingError> {
            Ok(vec![])
        }
        async fn change(
            &self,
            _tenant_id: TenantId,
            _book_id: BookId,
            _revision: i64,
            change: BookChange,
            _audit: Option<AuditRecord>,
        ) -> Result<Book, AccountingError> {
            let mut book = self.book.lock().unwrap().clone();
            change(&mut book).await?;
            Ok(book)
        }
        async fn assign(
            &self,
            _tenant_id: TenantId,
            _command: AssignCommand,
            _now: DateTime<Utc>,
        ) -> Result<Posting, AccountingError> {
            unimplemented!()
        }
        async fn transition(
            &self,
            _tenant_id: TenantId,
            _posting_id: PostingId,
            _change: PostingChange,
            _audit: Option<AuditRecord>,
        ) -> Result<Posting, AccountingError> {
            unimplemented!()
        }
    }

    fn dummy_tx_manager() -> TxManager {
        let pool =
            sqlx::PgPool::connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none").unwrap();
        TxManager::new(pool, sanvi_testsupport::test_ids())
    }

    fn dummy_clock() -> Arc<dyn Clock> {
        Arc::new(sanvi_kernel::SystemClock)
    }

    fn dummy_ids() -> Arc<dyn RawIdGenerator> {
        sanvi_testsupport::test_ids()
    }

    #[tokio::test]
    async fn empty_secrets_compose_without_consent_and_verification_fails_closed() {
        let config = AccountingConfig::default();
        let authorizer: Arc<dyn Authorizer> = Arc::new(StubAuthorizer {
            recorded: Mutex::new(Vec::new()),
            error_on_check: false,
        });
        let tenant_id = TenantId::try_from(10).unwrap();
        let user_id = UserId::try_from(20).unwrap();
        let book_id = BookId::try_from(100).unwrap();
        let conn_id = ConnectionId::try_from(200).unwrap();

        let mut book = Book::new(book_id, tenant_id, "entity_1".into()).unwrap();
        let connection = Connection::new(
            conn_id,
            Provider::Freee,
            "company_1".into(),
            AccountMappings {
                revenue_account: "rev".into(),
                refund_account: "ref".into(),
                clearing_account: "clr".into(),
                tax_codes: BTreeMap::from([("tax".into(), "tax".into())]),
            },
        )
        .unwrap();
        book.add_connection(connection).unwrap();

        let store: Arc<dyn AccountingStore> = Arc::new(StubStore {
            book: Mutex::new(book),
        });

        let state = build_accounting_state_with_store(
            &config,
            store,
            dummy_tx_manager(),
            dummy_ids(),
            authorizer,
            dummy_clock(),
        )
        .expect("empty config should compose without error");

        assert!(state.consent.is_none());

        let actor = Actor::User { tenant_id, user_id };
        let res = state
            .activate_book
            .execute(
                actor,
                RequestContext::default(),
                book_id,
                1,
                conn_id,
                Utc::now(),
            )
            .await;

        assert_eq!(res.unwrap_err(), AccountingError::VerificationUnavailable);
    }

    #[tokio::test]
    async fn malformed_kek_is_a_boot_error() {
        let config = AccountingConfig {
            freee_client_id: "client-id".into(),
            freee_client_secret: Redacted::new("secret".into()),
            freee_redirect_uri: "https://app.sanvi.test/callback".into(),
            oauth_state_signing_secret: Redacted::new("state-secret".into()),
            token_kek: Redacted::new("not-base64".into()),
            token_kek_id: "kek-1".into(),
        };
        let authorizer: Arc<dyn Authorizer> = Arc::new(StubAuthorizer {
            recorded: Mutex::new(Vec::new()),
            error_on_check: false,
        });

        let err = build_accounting_state(
            &config,
            dummy_tx_manager(),
            dummy_ids(),
            authorizer,
            dummy_clock(),
        )
        .unwrap_err()
        .to_string();

        assert!(
            err.contains("token_kek"),
            "error must name token_kek: {err}"
        );
        assert!(
            !err.contains("not-base64"),
            "error must not leak secret value: {err}"
        );
    }

    #[tokio::test]
    async fn empty_redirect_uri_with_secrets_is_a_boot_error() {
        let kek_base64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [7u8; 32]);
        let config = AccountingConfig {
            freee_client_id: "client-id".into(),
            freee_client_secret: Redacted::new("secret".into()),
            freee_redirect_uri: "".into(),
            oauth_state_signing_secret: Redacted::new("state-secret".into()),
            token_kek: Redacted::new(kek_base64),
            token_kek_id: "kek-1".into(),
        };
        let authorizer: Arc<dyn Authorizer> = Arc::new(StubAuthorizer {
            recorded: Mutex::new(Vec::new()),
            error_on_check: false,
        });

        let res = build_accounting_state(
            &config,
            dummy_tx_manager(),
            dummy_ids(),
            authorizer,
            dummy_clock(),
        );

        assert!(
            res.is_err(),
            "empty redirect uri with secrets present must be a boot error"
        );
    }

    #[tokio::test]
    async fn access_maps_permissions_and_denies_export() {
        let authorizer = Arc::new(StubAuthorizer {
            recorded: Mutex::new(Vec::new()),
            error_on_check: false,
        });
        let access = AuthorizerAccess(authorizer.clone());
        let tenant_id = TenantId::try_from(1).unwrap();
        let user_id = UserId::try_from(2).unwrap();
        let user = Actor::User { tenant_id, user_id };
        let system = Actor::System {
            tenant_id,
            service: "worker",
        };

        access
            .require(user, AccountingPermission::Read)
            .await
            .unwrap();
        access
            .require(user, AccountingPermission::Manage)
            .await
            .unwrap();

        assert_eq!(
            access
                .require(user, AccountingPermission::Export)
                .await
                .unwrap_err(),
            AccountingError::Forbidden
        );
        assert_eq!(
            access
                .require(system, AccountingPermission::Read)
                .await
                .unwrap_err(),
            AccountingError::Forbidden
        );
        assert_eq!(
            access
                .require(system, AccountingPermission::Manage)
                .await
                .unwrap_err(),
            AccountingError::Forbidden
        );

        let calls = authorizer.recorded.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                ("tenant.accounting.read".to_string(), false),
                ("tenant.accounting.manage".to_string(), true),
            ]
        );

        let failing_authorizer = Arc::new(StubAuthorizer {
            recorded: Mutex::new(Vec::new()),
            error_on_check: true,
        });
        let access_failing = AuthorizerAccess(failing_authorizer);
        assert_eq!(
            access_failing
                .require(user, AccountingPermission::Read)
                .await
                .unwrap_err(),
            AccountingError::ProviderUnavailable
        );
    }
}
