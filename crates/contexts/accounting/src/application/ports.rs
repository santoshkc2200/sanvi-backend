use std::{future::Future, pin::Pin};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_kernel::{EventId, TenantId, UserId};

use crate::domain::{
    AccountingError, AccountingEvent, Book, BookId, Connection, ConnectionId, Credential,
    OAuthStateClaims, Posting, PostingId, Provider, ProviderCatalog, Secret, TokenGrant,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    User {
        tenant_id: TenantId,
        user_id: UserId,
    },
    System {
        tenant_id: TenantId,
        service: &'static str,
    },
}

impl Actor {
    pub fn tenant_id(&self) -> TenantId {
        match *self {
            Actor::User { tenant_id, .. } | Actor::System { tenant_id, .. } => tenant_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Read,
    Manage,
    Export,
}

#[async_trait]
pub trait AccountingAccess: Send + Sync {
    async fn require(&self, actor: Actor, permission: Permission) -> Result<(), AccountingError>;
}

/// A trusted adapter must validate tenant-bound credentials, organization access
/// and all account/tax mappings. A submitted organization ID is not verification.
#[async_trait]
pub trait ProviderVerifier: Send + Sync {
    async fn verify(
        &self,
        tenant_id: TenantId,
        connection: &Connection,
    ) -> Result<(), AccountingError>;
}

pub type BookChange = Box<dyn FnOnce(&mut Book) -> Result<AccountingEvent, AccountingError> + Send>;

pub type PostingChange = Box<dyn FnOnce(&mut Posting) -> Result<bool, AccountingError> + Send>;

/// Each mutation is one transaction, including its outbox event. `change` and
/// `assign` serialize on the same book row; pending includes uncertain exports.
#[async_trait]
pub trait AccountingStore: Send + Sync {
    async fn create(&self, book: Book, event: AccountingEvent) -> Result<Book, AccountingError>;
    /// Unlocked snapshot; mutations must pass its revision back to `change`.
    async fn get(&self, tenant_id: TenantId, book_id: BookId) -> Result<Book, AccountingError>;
    async fn change(
        &self,
        tenant_id: TenantId,
        book_id: BookId,
        revision: i64,
        change: BookChange,
    ) -> Result<Book, AccountingError>;
    async fn assign(
        &self,
        tenant_id: TenantId,
        command: AssignCommand,
        now: DateTime<Utc>,
    ) -> Result<Posting, AccountingError>;
    async fn transition(
        &self,
        tenant_id: TenantId,
        posting_id: PostingId,
        change: PostingChange,
    ) -> Result<Posting, AccountingError>;
}

#[derive(Debug, Clone)]
pub struct AssignCommand {
    pub book_id: BookId,
    pub source_event_id: EventId,
    /// SHA-256 of canonical source content; reuse with changed content conflicts.
    pub fingerprint: String,
    pub accounting_at: DateTime<Utc>,
}

/// One provider's OAuth client. Errors map to `ReauthorizationRequired` when
/// the provider refuses a grant, `ProviderUnavailable` when it cannot be
/// reached or is rate limiting, and `AuthorizationRejected` otherwise.
#[async_trait]
pub trait ProviderOAuth: Send + Sync {
    fn provider(&self) -> Provider;
    /// The consent URL carrying `state`; it limits consent to one organization.
    fn authorization_url(&self, state: &str) -> String;
    async fn exchange_code(&self, code: &str) -> Result<TokenGrant, AccountingError>;
    async fn refresh(&self, refresh_token: &Secret) -> Result<TokenGrant, AccountingError>;
    async fn revoke(&self, refresh_token: &Secret) -> Result<(), AccountingError>;
}

/// Read-only provider lookups used to verify a connection's mappings.
#[async_trait]
pub trait ProviderDirectory: Send + Sync {
    fn provider(&self) -> Provider;
    async fn catalog(
        &self,
        access_token: &Secret,
        organization_id: &str,
    ) -> Result<ProviderCatalog, AccountingError>;
}

pub type CredentialChange = Box<
    dyn FnOnce(
            Credential,
        ) -> Pin<Box<dyn Future<Output = Result<Credential, AccountingError>> + Send>>
        + Send,
>;

/// Encrypted credential persistence and single-use OAuth nonces.
#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn issue_state(&self, claims: &OAuthStateClaims) -> Result<(), AccountingError>;
    /// Atomically burn a nonce; `false` if unknown or already used. Expiry is
    /// checked by the caller's clock against the signed `expires_at`.
    async fn consume_state(&self, claims: &OAuthStateClaims) -> Result<bool, AccountingError>;
    /// Insert or replace the connection's credential with a fresh consent.
    async fn authorize(
        &self,
        tenant_id: TenantId,
        credential: Credential,
    ) -> Result<(), AccountingError>;
    /// Lock the credential row for the whole of `change`, which may call the
    /// provider, then persist its result in the same transaction. A status
    /// change writes its outbox event in that transaction too.
    async fn update(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectionId,
        change: CredentialChange,
    ) -> Result<Credential, AccountingError>;
}
