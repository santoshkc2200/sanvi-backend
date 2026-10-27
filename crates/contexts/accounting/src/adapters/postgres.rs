use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_kernel::{EventEnvelope, EventId, RawIdGenerator, TenantId, UserId};
use sanvi_messaging::OutboxWriter;
use sanvi_persistence::{ScopedTx, TxManager};

use crate::{
    application::{
        AccountingStore, AssignCommand,
        ports::{AuditRecord, BookChange, PostingChange},
    },
    domain::{
        AccountingError, AccountingEvent, Book, BookId, ConnectionId, CredentialStatus, Posting,
        PostingId, PostingStatus,
    },
};

fn storage(error: impl std::fmt::Display) -> AccountingError {
    AccountingError::Storage(error.to_string())
}

struct BookRow {
    document: serde_json::Value,
    revision: i64,
    legal_entity_key: String,
}

struct BookListRow {
    id: i64,
    document: serde_json::Value,
    revision: i64,
    legal_entity_key: String,
}

struct CredentialStatusRow {
    connection_id: i64,
    status: String,
}

pub struct PostgresAccountingStore {
    transactions: TxManager,
    ids: Arc<dyn RawIdGenerator>,
}

impl PostgresAccountingStore {
    pub fn new(transactions: TxManager, ids: Arc<dyn RawIdGenerator>) -> Self {
        Self { transactions, ids }
    }

    async fn begin(&self, tenant: TenantId) -> Result<ScopedTx<'_>, AccountingError> {
        self.transactions
            .begin_for(tenant.as_i64())
            .await
            .map_err(storage)
    }

    /// Lock the book row; every mutation and assignment serializes on it.
    /// `FOR NO KEY UPDATE` still excludes other lockers but admits the
    /// `FOR KEY SHARE` that a foreign-key check takes, so a nested credential
    /// transaction can insert rows referencing this book while it is held.
    async fn load(
        scope: &mut ScopedTx<'_>,
        tenant: TenantId,
        id: BookId,
    ) -> Result<Book, AccountingError> {
        let row = sqlx::query_as!(
            BookRow,
            "SELECT document, revision, legal_entity_key FROM accounting.books WHERE tenant_id = $1 AND id = $2 FOR NO KEY UPDATE",
            tenant.as_i64(), id.as_i64()
        ).fetch_optional(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?.ok_or(AccountingError::NotFound)?;
        Self::decode(row, tenant, id)
    }

    /// Read a snapshot without blocking assignments; callers recheck revisions.
    async fn read(
        scope: &mut ScopedTx<'_>,
        tenant: TenantId,
        id: BookId,
    ) -> Result<Book, AccountingError> {
        let row = sqlx::query_as!(
            BookRow,
            "SELECT document, revision, legal_entity_key FROM accounting.books WHERE tenant_id = $1 AND id = $2",
            tenant.as_i64(), id.as_i64()
        ).fetch_optional(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?.ok_or(AccountingError::NotFound)?;
        Self::decode(row, tenant, id)
    }

    fn decode(row: BookRow, tenant: TenantId, id: BookId) -> Result<Book, AccountingError> {
        let book: Book = serde_json::from_value(row.document).map_err(storage)?;
        book.validate()?;
        if book.tenant_id() != tenant
            || book.id() != id
            || book.revision() != row.revision
            || book.legal_entity_key() != row.legal_entity_key
        {
            return Err(AccountingError::Storage(
                "book identity/revision mismatch".into(),
            ));
        }
        Ok(book)
    }

    async fn append(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant: TenantId,
        event: AccountingEvent,
    ) -> Result<(), AccountingError> {
        let id = self
            .ids
            .next_id()
            .map(EventId::from_snowflake)
            .map_err(storage)?;
        let envelope = EventEnvelope::new(
            id,
            "accounting.book",
            event.book_id.as_snowflake(),
            format!("accounting.{}", event.operation),
            serde_json::to_value(&event).map_err(storage)?,
        )
        .with_metadata("tenant_id", serde_json::json!(tenant.to_string()));
        OutboxWriter
            .append(scope.tx_mut(), &envelope)
            .await
            .map_err(storage)
    }

    async fn find_posting(
        scope: &mut ScopedTx<'_>,
        tenant: TenantId,
        book: BookId,
        source: EventId,
    ) -> Result<Option<Posting>, AccountingError> {
        let row = sqlx::query!(
            "SELECT id, tenant_id, book_id, connection_id, source_event_id, fingerprint, accounting_at, status, attempts, failure_reason, resolution_note, resolved_by, status_changed_at, external_record_id FROM accounting.postings WHERE tenant_id = $1 AND book_id = $2 AND source_event_id = $3",
            tenant.as_i64(), book.as_i64(), source.as_i64()
        ).fetch_optional(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?;
        row.map(|r| {
            Self::decode_posting(
                r.id,
                r.tenant_id,
                r.book_id,
                r.connection_id,
                r.source_event_id,
                r.fingerprint,
                r.accounting_at,
                r.status,
                r.attempts,
                r.failure_reason,
                r.resolution_note,
                r.resolved_by,
                r.status_changed_at,
                r.external_record_id,
            )
        })
        .transpose()
    }

    async fn lock_posting(
        scope: &mut ScopedTx<'_>,
        tenant: TenantId,
        id: PostingId,
    ) -> Result<Posting, AccountingError> {
        let r = sqlx::query!(
            "SELECT id, tenant_id, book_id, connection_id, source_event_id, fingerprint, accounting_at, status, attempts, failure_reason, resolution_note, resolved_by, status_changed_at, external_record_id FROM accounting.postings WHERE tenant_id = $1 AND id = $2 FOR UPDATE",
            tenant.as_i64(), id.as_i64()
        ).fetch_optional(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?.ok_or(AccountingError::NotFound)?;
        Self::decode_posting(
            r.id,
            r.tenant_id,
            r.book_id,
            r.connection_id,
            r.source_event_id,
            r.fingerprint,
            r.accounting_at,
            r.status,
            r.attempts,
            r.failure_reason,
            r.resolution_note,
            r.resolved_by,
            r.status_changed_at,
            r.external_record_id,
        )
    }

    fn decode_status(
        status: &str,
        external_record_id: Option<String>,
        failure_reason: Option<String>,
        resolution_note: Option<String>,
        resolved_by: Option<i64>,
    ) -> Result<PostingStatus, AccountingError> {
        match status {
            "pending" => {
                if external_record_id.is_none()
                    && resolution_note.is_none()
                    && resolved_by.is_none()
                {
                    Ok(PostingStatus::Pending)
                } else {
                    Err(AccountingError::Storage(
                        "invalid pending posting row".into(),
                    ))
                }
            }
            "submitted" => {
                if external_record_id.is_none()
                    && resolution_note.is_none()
                    && resolved_by.is_none()
                {
                    Ok(PostingStatus::Submitted)
                } else {
                    Err(AccountingError::Storage(
                        "invalid submitted posting row".into(),
                    ))
                }
            }
            "delivered" => {
                if let Some(external_record_id) = external_record_id {
                    if resolution_note.is_none() && resolved_by.is_none() {
                        Ok(PostingStatus::Delivered { external_record_id })
                    } else {
                        Err(AccountingError::Storage(
                            "invalid delivered posting row".into(),
                        ))
                    }
                } else {
                    Err(AccountingError::Storage(
                        "delivered posting missing external_record_id".into(),
                    ))
                }
            }
            "failed" => {
                if let Some(reason) = failure_reason {
                    if external_record_id.is_none()
                        && resolution_note.is_none()
                        && resolved_by.is_none()
                    {
                        Ok(PostingStatus::Failed { reason })
                    } else {
                        Err(AccountingError::Storage(
                            "invalid failed posting row".into(),
                        ))
                    }
                } else {
                    Err(AccountingError::Storage(
                        "failed posting missing failure_reason".into(),
                    ))
                }
            }
            "resolved_manually" => match (resolution_note, resolved_by) {
                (Some(note), Some(by)) => {
                    let resolved_by = UserId::try_from(by).map_err(storage)?;
                    Ok(PostingStatus::ResolvedManually {
                        note,
                        external_record_id,
                        resolved_by,
                    })
                }
                _ => Err(AccountingError::Storage(
                    "resolved_manually posting missing note or user".into(),
                )),
            },
            _ => Err(AccountingError::Storage(format!(
                "unknown posting status: {status}"
            ))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_posting(
        id: i64,
        tenant_id: i64,
        book_id: i64,
        connection_id: i64,
        source_event_id: i64,
        fingerprint: String,
        accounting_at: DateTime<Utc>,
        status: String,
        attempts: i32,
        failure_reason: Option<String>,
        resolution_note: Option<String>,
        resolved_by: Option<i64>,
        status_changed_at: DateTime<Utc>,
        external_record_id: Option<String>,
    ) -> Result<Posting, AccountingError> {
        let status = Self::decode_status(
            &status,
            external_record_id,
            failure_reason,
            resolution_note,
            resolved_by,
        )?;
        Ok(Posting {
            id: PostingId::try_from(id).map_err(storage)?,
            tenant_id: TenantId::try_from(tenant_id).map_err(storage)?,
            book_id: BookId::try_from(book_id).map_err(storage)?,
            connection_id: ConnectionId::try_from(connection_id).map_err(storage)?,
            source_event_id: EventId::try_from(source_event_id).map_err(storage)?,
            fingerprint,
            accounting_at,
            status,
            attempts,
            status_changed_at,
        })
    }
}

#[async_trait]
impl AccountingStore for PostgresAccountingStore {
    async fn create(
        &self,
        book: Book,
        event: AccountingEvent,
        audit: AuditRecord,
    ) -> Result<Book, AccountingError> {
        let mut scope = self.begin(book.tenant_id()).await?;
        let document = serde_json::to_value(&book).map_err(storage)?;
        let inserted = sqlx::query!(
            "INSERT INTO accounting.books (id, tenant_id, legal_entity_key, revision, document) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (tenant_id, legal_entity_key) DO NOTHING",
            book.id().as_i64(), book.tenant_id().as_i64(), book.legal_entity_key(), book.revision(), document
        ).execute(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?;
        if inserted.rows_affected() == 0 {
            return Err(AccountingError::Conflict(
                "legal entity already has an accounting book",
            ));
        }
        self.append(&mut scope, book.tenant_id(), event).await?;
        crate::adapters::audit::record(&mut scope, &*self.ids, &audit).await?;
        scope.commit().await.map_err(storage)?;
        Ok(book)
    }

    async fn get(&self, tenant: TenantId, book: BookId) -> Result<Book, AccountingError> {
        let mut scope = self.begin(tenant).await?;
        let book = Self::read(&mut scope, tenant, book).await?;
        scope.commit().await.map_err(storage)?;
        Ok(book)
    }

    async fn list(&self, tenant: TenantId) -> Result<Vec<Book>, AccountingError> {
        let mut scope = self.begin(tenant).await?;
        let rows = sqlx::query_as!(
            BookListRow,
            "SELECT id, document, revision, legal_entity_key FROM accounting.books WHERE tenant_id = $1 ORDER BY id ASC LIMIT $2",
            tenant.as_i64(),
            crate::application::LIST_BOOKS_LIMIT
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        let mut books = Vec::with_capacity(rows.len());
        for row in rows {
            let id = BookId::try_from(row.id).map_err(storage)?;
            let book_row = BookRow {
                document: row.document,
                revision: row.revision,
                legal_entity_key: row.legal_entity_key,
            };
            books.push(Self::decode(book_row, tenant, id)?);
        }
        scope.commit().await.map_err(storage)?;
        Ok(books)
    }

    async fn credential_statuses(
        &self,
        tenant: TenantId,
        book_id: BookId,
    ) -> Result<Vec<(ConnectionId, CredentialStatus)>, AccountingError> {
        let mut scope = self.begin(tenant).await?;
        let rows = sqlx::query_as!(
            CredentialStatusRow,
            "SELECT connection_id, status FROM accounting.provider_credentials WHERE tenant_id = $1 AND book_id = $2 ORDER BY connection_id ASC",
            tenant.as_i64(),
            book_id.as_i64()
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        let mut statuses = Vec::with_capacity(rows.len());
        for row in rows {
            let connection_id = ConnectionId::try_from(row.connection_id).map_err(storage)?;
            let status = CredentialStatus::parse(&row.status).ok_or_else(|| {
                AccountingError::Storage(format!("unknown credential status: {}", row.status))
            })?;
            statuses.push((connection_id, status));
        }
        scope.commit().await.map_err(storage)?;
        Ok(statuses)
    }

    async fn change(
        &self,
        tenant_id: TenantId,
        book_id: BookId,
        revision: i64,
        change: BookChange,
        audit: Option<AuditRecord>,
    ) -> Result<Book, AccountingError> {
        let mut scope = self.begin(tenant_id).await?;
        let mut book = Self::load(&mut scope, tenant_id, book_id).await?;
        if book.revision() != revision {
            return Err(AccountingError::StaleRevision);
        }
        let Some(event) = change(&mut book).await? else {
            scope.commit().await.map_err(storage)?;
            return Ok(book);
        };
        let document = serde_json::to_value(&book).map_err(storage)?;
        sqlx::query!("UPDATE accounting.books SET revision = $3, document = $4 WHERE tenant_id = $1 AND id = $2",
            tenant_id.as_i64(), book_id.as_i64(), book.revision(), document
        ).execute(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?;
        self.append(&mut scope, tenant_id, event).await?;
        if let Some(audit) = audit {
            crate::adapters::audit::record(&mut scope, &*self.ids, &audit).await?;
        }
        scope.commit().await.map_err(storage)?;
        Ok(book)
    }

    async fn assign(
        &self,
        tenant_id: TenantId,
        command: AssignCommand,
        now: DateTime<Utc>,
    ) -> Result<Posting, AccountingError> {
        let mut scope = self.begin(tenant_id).await?;
        let book = Self::load(&mut scope, tenant_id, command.book_id).await?;
        // Dedup before routing: retries after a switch retain their original destination.
        if let Some(existing) = Self::find_posting(
            &mut scope,
            tenant_id,
            command.book_id,
            command.source_event_id,
        )
        .await?
        {
            if existing.fingerprint != command.fingerprint
                || existing.accounting_at != command.accounting_at
            {
                return Err(AccountingError::IdempotencyConflict);
            }
            scope.commit().await.map_err(storage)?;
            return Ok(existing);
        }
        let connection = book.destination_for(command.accounting_at, now)?;
        let posting = Posting::new_pending(
            PostingId::from_snowflake(self.ids.next_id().map_err(storage)?),
            tenant_id,
            command.book_id,
            connection,
            command.source_event_id,
            command.fingerprint,
            command.accounting_at,
            now,
        );
        sqlx::query!(
            "INSERT INTO accounting.postings (id, tenant_id, book_id, connection_id, source_event_id, fingerprint, accounting_at, status, attempts, status_changed_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            posting.id.as_i64(), posting.tenant_id.as_i64(), posting.book_id.as_i64(), posting.connection_id.as_i64(),
            posting.source_event_id.as_i64(), posting.fingerprint, posting.accounting_at, posting.status.as_str(),
            posting.attempts, posting.status_changed_at
        ).execute(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?;
        self.append(
            &mut scope,
            tenant_id,
            AccountingEvent {
                book_id: book.id(),
                revision: book.revision(),
                operation: "posting_assigned",
                posting_id: Some(posting.id),
            },
        )
        .await?;
        scope.commit().await.map_err(storage)?;
        Ok(posting)
    }

    async fn transition(
        &self,
        tenant_id: TenantId,
        posting_id: PostingId,
        change: PostingChange,
        audit: Option<AuditRecord>,
    ) -> Result<Posting, AccountingError> {
        let mut scope = self.begin(tenant_id).await?;
        let parent = sqlx::query!(
            "SELECT book_id FROM accounting.postings WHERE tenant_id = $1 AND id = $2",
            tenant_id.as_i64(),
            posting_id.as_i64()
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?
        .ok_or(AccountingError::NotFound)?;
        let book = Self::load(
            &mut scope,
            tenant_id,
            BookId::try_from(parent.book_id).map_err(storage)?,
        )
        .await?;
        if audit
            .as_ref()
            .is_some_and(|audit| audit.book_id != book.id())
        {
            return Err(AccountingError::NotFound);
        }
        let mut posting = Self::lock_posting(&mut scope, tenant_id, posting_id).await?;
        let applied = change(&mut posting)?;
        if applied {
            let (external_record_id, failure_reason, resolution_note, resolved_by) =
                match &posting.status {
                    PostingStatus::Pending | PostingStatus::Submitted => (None, None, None, None),
                    PostingStatus::Delivered { external_record_id } => {
                        (Some(external_record_id.as_str()), None, None, None)
                    }
                    PostingStatus::Failed { reason } => (None, Some(reason.as_str()), None, None),
                    PostingStatus::ResolvedManually {
                        note,
                        external_record_id,
                        resolved_by,
                    } => (
                        external_record_id.as_deref(),
                        None,
                        Some(note.as_str()),
                        Some(resolved_by.as_i64()),
                    ),
                };
            sqlx::query!(
                "UPDATE accounting.postings SET status = $3, attempts = $4, failure_reason = $5, resolution_note = $6, resolved_by = $7, status_changed_at = $8, external_record_id = $9 WHERE tenant_id = $1 AND id = $2",
                tenant_id.as_i64(),
                posting.id.as_i64(),
                posting.status.as_str(),
                posting.attempts,
                failure_reason,
                resolution_note,
                resolved_by,
                posting.status_changed_at,
                external_record_id,
            ).execute(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?;

            self.append(
                &mut scope,
                tenant_id,
                AccountingEvent {
                    book_id: book.id(),
                    revision: book.revision(),
                    operation: posting.status.operation(),
                    posting_id: Some(posting.id),
                },
            )
            .await?;

            if let Some(audit) = audit {
                crate::adapters::audit::record(&mut scope, &*self.ids, &audit).await?;
            }
        }
        scope.commit().await.map_err(storage)?;
        Ok(posting)
    }
}
