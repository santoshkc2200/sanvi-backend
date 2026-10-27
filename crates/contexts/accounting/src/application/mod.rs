//! Authorized use cases. Provider consent lives in `authorization`; there is
//! no posting exporter yet.
pub mod authorization;
pub mod ports;

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sanvi_kernel::{Clock, RawIdGenerator};

use crate::domain::{
    AccountingError, AccountingEvent, Book, BookId, Connection, ConnectionId, Posting, PostingId,
};
pub use authorization::{
    AccessTokens, Authorization, CompleteAuthorization, CredentialVerifier, RevokeAuthorization,
    StartAuthorization,
};
pub use ports::{
    AccountingAccess, AccountingStore, Actor, AssignCommand, AuditRecord, CredentialStore,
    Permission, ProviderDirectory, ProviderOAuth, ProviderVerifier, RequestContext,
};

pub struct Services {
    pub store: Arc<dyn AccountingStore>,
    pub access: Arc<dyn AccountingAccess>,
    pub verifier: Arc<dyn ProviderVerifier>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn RawIdGenerator>,
}

impl Services {
    async fn authorize(&self, actor: Actor, permission: Permission) -> Result<(), AccountingError> {
        match (permission, actor) {
            (Permission::Export, Actor::User { .. }) => Err(AccountingError::Forbidden),
            (Permission::Read | Permission::Manage, Actor::System { .. }) => {
                Err(AccountingError::Forbidden)
            }
            (Permission::Read | Permission::Manage, Actor::User { .. }) => {
                self.access.require(actor, permission).await
            }
            (Permission::Export, Actor::System { .. }) => Ok(()),
        }
    }
}

fn event(book: &Book, operation: &'static str) -> AccountingEvent {
    AccountingEvent {
        book_id: book.id(),
        revision: book.revision(),
        operation,
        posting_id: None,
    }
}

/// PostgreSQL timestamps retain microseconds. Normalize every boundary instant so
/// a stored value compares equal to the one a caller supplied or retries with.
fn to_micros(at: DateTime<Utc>) -> Result<DateTime<Utc>, AccountingError> {
    DateTime::from_timestamp_micros(at.timestamp_micros())
        .ok_or(AccountingError::Invalid("timestamp is out of range"))
}

pub(crate) fn audit_record(
    actor: Actor,
    request: RequestContext,
    book_id: BookId,
    action: &'static str,
    after: serde_json::Value,
    now: DateTime<Utc>,
) -> Result<AuditRecord, AccountingError> {
    match actor {
        Actor::User { tenant_id, user_id } => Ok(AuditRecord {
            tenant_id,
            user_id,
            book_id,
            action,
            occurred_at: now,
            after,
            request,
        }),
        Actor::System { .. } => Err(AccountingError::Forbidden),
    }
}

/// HTTP callers must obtain this actor from authenticated request context.
pub struct CreateBook(pub Arc<Services>);
impl CreateBook {
    pub async fn execute(
        &self,
        actor: Actor,
        request: RequestContext,
        legal_entity_key: String,
    ) -> Result<Book, AccountingError> {
        self.0.authorize(actor, Permission::Manage).await?;
        let id = self
            .0
            .ids
            .next_id()
            .map(BookId::from_snowflake)
            .map_err(|error| AccountingError::Storage(error.to_string()))?;
        let book = Book::new(id, actor.tenant_id(), legal_entity_key)?;
        let now = self.0.clock.now();
        let audit = audit_record(
            actor,
            request,
            book.id(),
            "accounting.book.created",
            serde_json::json!({
                "book_id": book.id().to_string(),
                "legal_entity_key": book.legal_entity_key(),
            }),
            now,
        )?;
        let created = event(&book, "book_created");
        self.0.store.create(book, created, audit).await
    }
}

pub struct GetBook(pub Arc<Services>);
impl GetBook {
    pub async fn execute(&self, actor: Actor, book_id: BookId) -> Result<Book, AccountingError> {
        self.0.authorize(actor, Permission::Read).await?;
        self.0.store.get(actor.tenant_id(), book_id).await
    }
}

/// Register metadata only. This does not assert that OAuth succeeded.
pub struct RegisterConnection(pub Arc<Services>);
impl RegisterConnection {
    pub async fn execute(
        &self,
        actor: Actor,
        request: RequestContext,
        book_id: BookId,
        revision: i64,
        connection: Connection,
    ) -> Result<Book, AccountingError> {
        self.0.authorize(actor, Permission::Manage).await?;
        let now = self.0.clock.now();
        let audit = audit_record(
            actor,
            request,
            book_id,
            "accounting.connection.registered",
            serde_json::json!({
                "book_id": book_id.to_string(),
                "connection_id": connection.id().to_string(),
                "provider": connection.provider().as_str(),
                "organization_id": connection.organization_id(),
                "mappings": connection.mappings(),
            }),
            now,
        )?;
        self.0
            .store
            .change(
                actor.tenant_id(),
                book_id,
                revision,
                Box::new(move |book| {
                    Box::pin(async move {
                        book.add_connection(connection)?;
                        Ok(Some(event(book, "connection_registered")))
                    })
                }),
                Some(audit),
            )
            .await
    }
}

pub struct ActivateBook(pub Arc<Services>);
impl ActivateBook {
    pub async fn execute(
        &self,
        actor: Actor,
        request: RequestContext,
        book_id: BookId,
        revision: i64,
        connection_id: ConnectionId,
        starts_at: DateTime<Utc>,
    ) -> Result<Book, AccountingError> {
        self.0.authorize(actor, Permission::Manage).await?;
        let starts_at = to_micros(starts_at)?;
        let book = self.0.store.get(actor.tenant_id(), book_id).await?;
        if book.revision() != revision {
            return Err(AccountingError::StaleRevision);
        }
        let now = self.0.clock.now();
        let audit = audit_record(
            actor,
            request,
            book_id,
            "accounting.destination.activated",
            serde_json::json!({
                "book_id": book_id.to_string(),
                "connection_id": connection_id.to_string(),
                "starts_at": starts_at,
            }),
            now,
        )?;
        let services = self.0.clone();
        self.0
            .store
            .change(
                actor.tenant_id(),
                book_id,
                revision,
                Box::new(move |book| {
                    Box::pin(async move {
                        services
                            .verifier
                            .verify(actor.tenant_id(), book.connection(connection_id)?)
                            .await?;
                        let now = services.clock.now();
                        book.activate_initial(connection_id, starts_at, now)?;
                        Ok(Some(event(book, "destination_activated")))
                    })
                }),
                Some(audit),
            )
            .await
    }
}

pub struct ScheduleSwitch(pub Arc<Services>);
impl ScheduleSwitch {
    pub async fn execute(
        &self,
        actor: Actor,
        request: RequestContext,
        book_id: BookId,
        revision: i64,
        connection_id: ConnectionId,
        cutover_at: DateTime<Utc>,
    ) -> Result<Book, AccountingError> {
        self.0.authorize(actor, Permission::Manage).await?;
        let cutover_at = to_micros(cutover_at)?;
        let book = self.0.store.get(actor.tenant_id(), book_id).await?;
        if book.revision() != revision {
            return Err(AccountingError::StaleRevision);
        }
        let now = self.0.clock.now();
        let audit = audit_record(
            actor,
            request,
            book_id,
            "accounting.switch.scheduled",
            serde_json::json!({
                "book_id": book_id.to_string(),
                "connection_id": connection_id.to_string(),
                "cutover_at": cutover_at,
            }),
            now,
        )?;
        let services = self.0.clone();
        self.0
            .store
            .change(
                actor.tenant_id(),
                book_id,
                revision,
                Box::new(move |book| {
                    Box::pin(async move {
                        services
                            .verifier
                            .verify(actor.tenant_id(), book.connection(connection_id)?)
                            .await?;
                        let now = services.clock.now();
                        book.schedule_switch(connection_id, cutover_at, now)?;
                        Ok(Some(event(book, "switch_scheduled")))
                    })
                }),
                Some(audit),
            )
            .await
    }
}

pub struct CancelSwitch(pub Arc<Services>);
impl CancelSwitch {
    pub async fn execute(
        &self,
        actor: Actor,
        request: RequestContext,
        book_id: BookId,
        revision: i64,
    ) -> Result<Book, AccountingError> {
        self.0.authorize(actor, Permission::Manage).await?;
        let now = self.0.clock.now();
        let audit = audit_record(
            actor,
            request,
            book_id,
            "accounting.switch.canceled",
            serde_json::json!({
                "book_id": book_id.to_string(),
            }),
            now,
        )?;
        self.0
            .store
            .change(
                actor.tenant_id(),
                book_id,
                revision,
                Box::new(move |book| {
                    Box::pin(async move {
                        book.cancel_switch(now)?;
                        Ok(Some(event(book, "switch_canceled")))
                    })
                }),
                Some(audit),
            )
            .await
    }
}

pub struct AssignPosting(pub Arc<Services>);
impl AssignPosting {
    pub async fn execute(
        &self,
        actor: Actor,
        mut command: AssignCommand,
    ) -> Result<Posting, AccountingError> {
        self.0.authorize(actor, Permission::Export).await?;
        command.accounting_at = to_micros(command.accounting_at)?;
        if command.fingerprint.len() != 64
            || !command
                .fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(AccountingError::Invalid(
                "fingerprint must be lowercase SHA-256 hex",
            ));
        }
        self.0
            .store
            .assign(actor.tenant_id(), command, self.0.clock.now())
            .await
    }
}

pub struct MarkSubmitted(pub Arc<Services>);
impl MarkSubmitted {
    pub async fn execute(
        &self,
        actor: Actor,
        posting_id: PostingId,
    ) -> Result<Posting, AccountingError> {
        self.0.authorize(actor, Permission::Export).await?;
        let now = self.0.clock.now();
        self.0
            .store
            .transition(
                actor.tenant_id(),
                posting_id,
                Box::new(move |posting| posting.submit(now)),
                None,
            )
            .await
    }
}

pub struct RecordDelivery(pub Arc<Services>);
impl RecordDelivery {
    pub async fn execute(
        &self,
        actor: Actor,
        posting_id: PostingId,
        external_record_id: &str,
    ) -> Result<Posting, AccountingError> {
        self.0.authorize(actor, Permission::Export).await?;
        let ext = external_record_id.to_string();
        let now = self.0.clock.now();
        self.0
            .store
            .transition(
                actor.tenant_id(),
                posting_id,
                Box::new(move |posting| posting.deliver(ext, now)),
                None,
            )
            .await
    }
}

pub struct RecordFailure(pub Arc<Services>);
impl RecordFailure {
    pub async fn execute(
        &self,
        actor: Actor,
        posting_id: PostingId,
        reason: &str,
    ) -> Result<Posting, AccountingError> {
        self.0.authorize(actor, Permission::Export).await?;
        let reason = reason.to_string();
        let now = self.0.clock.now();
        self.0
            .store
            .transition(
                actor.tenant_id(),
                posting_id,
                Box::new(move |posting| posting.fail(reason, now)),
                None,
            )
            .await
    }
}

pub struct ResolvePosting(pub Arc<Services>);
impl ResolvePosting {
    pub async fn execute(
        &self,
        actor: Actor,
        request: RequestContext,
        book_id: BookId,
        posting_id: PostingId,
        note: &str,
        external_record_id: Option<&str>,
    ) -> Result<Posting, AccountingError> {
        self.0.authorize(actor, Permission::Manage).await?;
        let user_id = match actor {
            Actor::User { user_id, .. } => user_id,
            Actor::System { .. } => return Err(AccountingError::Forbidden),
        };
        let note = note.to_string();
        let ext = external_record_id.map(str::to_string);
        let now = self.0.clock.now();
        let audit = audit_record(
            actor,
            request,
            book_id,
            "accounting.posting.resolved",
            serde_json::json!({
                "posting_id": posting_id.to_string(),
                "resolution_note": note,
                "external_record_id": ext,
            }),
            now,
        )?;
        self.0
            .store
            .transition(
                actor.tenant_id(),
                posting_id,
                Box::new(move |posting| posting.resolve(note, ext, user_id, now)),
                Some(audit),
            )
            .await
    }
}
