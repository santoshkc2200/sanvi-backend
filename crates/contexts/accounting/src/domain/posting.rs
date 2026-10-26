use chrono::{DateTime, Utc};
use sanvi_kernel::{EventId, TenantId, UserId};
use serde::{Deserialize, Serialize};

use super::{AccountingError, BookId, ConnectionId, PostingId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PostingStatus {
    Pending,
    Submitted,
    Delivered {
        external_record_id: String,
    },
    Failed {
        reason: String,
    },
    ResolvedManually {
        note: String,
        external_record_id: Option<String>,
        resolved_by: UserId,
    },
}

impl PostingStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PostingStatus::Pending => "pending",
            PostingStatus::Submitted => "submitted",
            PostingStatus::Delivered { .. } => "delivered",
            PostingStatus::Failed { .. } => "failed",
            PostingStatus::ResolvedManually { .. } => "resolved_manually",
        }
    }

    pub fn operation(&self) -> &'static str {
        match self {
            PostingStatus::Pending => "posting_assigned",
            PostingStatus::Submitted => "posting_submitted",
            PostingStatus::Delivered { .. } => "posting_delivered",
            PostingStatus::Failed { .. } => "posting_failed",
            PostingStatus::ResolvedManually { .. } => "posting_resolved",
        }
    }
}

fn validate_bounded_text(
    text: &str,
    name: &'static str,
    max_bytes: usize,
) -> Result<(), AccountingError> {
    if text.trim().is_empty() || text.len() > max_bytes {
        Err(AccountingError::Invalid(name))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Posting {
    pub id: PostingId,
    pub tenant_id: TenantId,
    pub book_id: BookId,
    pub connection_id: ConnectionId,
    pub source_event_id: EventId,
    pub fingerprint: String,
    pub accounting_at: DateTime<Utc>,
    pub status: PostingStatus,
    pub attempts: i32,
    pub status_changed_at: DateTime<Utc>,
}

impl Posting {
    #[allow(clippy::too_many_arguments)]
    pub fn new_pending(
        id: PostingId,
        tenant_id: TenantId,
        book_id: BookId,
        connection_id: ConnectionId,
        source_event_id: EventId,
        fingerprint: String,
        accounting_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            tenant_id,
            book_id,
            connection_id,
            source_event_id,
            fingerprint,
            accounting_at,
            status: PostingStatus::Pending,
            attempts: 0,
            status_changed_at: now,
        }
    }

    pub fn submit(&mut self, now: DateTime<Utc>) -> Result<bool, AccountingError> {
        match &self.status {
            PostingStatus::Pending | PostingStatus::Failed { .. } => {
                self.status = PostingStatus::Submitted;
                self.attempts += 1;
                self.status_changed_at = now;
                Ok(true)
            }
            PostingStatus::Submitted => Ok(false),
            PostingStatus::Delivered { .. } | PostingStatus::ResolvedManually { .. } => Err(
                AccountingError::Conflict("posting status does not allow this transition"),
            ),
        }
    }

    pub fn deliver(
        &mut self,
        external_record_id: String,
        now: DateTime<Utc>,
    ) -> Result<bool, AccountingError> {
        validate_bounded_text(
            &external_record_id,
            "external record ID must contain 1–256 bytes",
            256,
        )?;
        match &self.status {
            PostingStatus::Submitted => {
                self.status = PostingStatus::Delivered { external_record_id };
                self.status_changed_at = now;
                Ok(true)
            }
            PostingStatus::Delivered {
                external_record_id: existing,
            } => {
                if existing == &external_record_id {
                    Ok(false)
                } else {
                    Err(AccountingError::Conflict(
                        "posting status does not allow this transition",
                    ))
                }
            }
            PostingStatus::Pending
            | PostingStatus::Failed { .. }
            | PostingStatus::ResolvedManually { .. } => Err(AccountingError::Conflict(
                "posting status does not allow this transition",
            )),
        }
    }

    pub fn fail(&mut self, reason: String, now: DateTime<Utc>) -> Result<bool, AccountingError> {
        validate_bounded_text(&reason, "failure reason must contain 1–1024 bytes", 1024)?;
        match &self.status {
            PostingStatus::Submitted => {
                self.status = PostingStatus::Failed { reason };
                self.status_changed_at = now;
                Ok(true)
            }
            PostingStatus::Failed { reason: existing } => {
                if existing == &reason {
                    Ok(false)
                } else {
                    Err(AccountingError::Conflict(
                        "posting status does not allow this transition",
                    ))
                }
            }
            PostingStatus::Pending
            | PostingStatus::Delivered { .. }
            | PostingStatus::ResolvedManually { .. } => Err(AccountingError::Conflict(
                "posting status does not allow this transition",
            )),
        }
    }

    pub fn resolve(
        &mut self,
        note: String,
        external_record_id: Option<String>,
        by: UserId,
        now: DateTime<Utc>,
    ) -> Result<bool, AccountingError> {
        validate_bounded_text(&note, "resolution note must contain 1–1024 bytes", 1024)?;
        if let Some(ext) = &external_record_id {
            validate_bounded_text(ext, "external record ID must contain 1–256 bytes", 256)?;
        }
        match &self.status {
            PostingStatus::Pending | PostingStatus::Submitted | PostingStatus::Failed { .. } => {
                self.status = PostingStatus::ResolvedManually {
                    note,
                    external_record_id,
                    resolved_by: by,
                };
                self.status_changed_at = now;
                Ok(true)
            }
            PostingStatus::ResolvedManually {
                note: existing_note,
                external_record_id: existing_ext,
                resolved_by: existing_by,
            } => {
                if existing_note == &note
                    && existing_ext == &external_record_id
                    && existing_by == &by
                {
                    Ok(false)
                } else {
                    Err(AccountingError::Conflict(
                        "posting status does not allow this transition",
                    ))
                }
            }
            PostingStatus::Delivered { .. } => Err(AccountingError::Conflict(
                "posting status does not allow this transition",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> DateTime<Utc> {
        "2026-10-26T00:00:00Z".parse().unwrap()
    }

    fn posting() -> Posting {
        Posting::new_pending(
            PostingId::try_from(1).unwrap(),
            TenantId::try_from(2).unwrap(),
            BookId::try_from(3).unwrap(),
            ConnectionId::try_from(4).unwrap(),
            EventId::try_from(5).unwrap(),
            "a".repeat(64),
            at(),
            at(),
        )
    }

    #[test]
    fn pending_submits_and_delivers() {
        let mut p = posting();
        assert_eq!(p.submit(at()), Ok(true));
        assert_eq!(p.attempts, 1);
        assert_eq!(p.status, PostingStatus::Submitted);
        assert_eq!(p.deliver("ext-1".into(), at()), Ok(true));
        assert_eq!(
            p.status,
            PostingStatus::Delivered {
                external_record_id: "ext-1".into()
            }
        );
        assert_eq!(p.deliver("ext-1".into(), at()), Ok(false));
        assert!(matches!(
            p.deliver("ext-2".into(), at()),
            Err(AccountingError::Conflict(_))
        ));
    }

    #[test]
    fn delivery_requires_submission() {
        let mut p = posting();
        assert!(matches!(
            p.deliver("ext-1".into(), at()),
            Err(AccountingError::Conflict(_))
        ));
    }

    #[test]
    fn retry_after_failure_counts_attempts() {
        let mut p = posting();
        assert_eq!(p.submit(at()), Ok(true));
        assert_eq!(p.fail("locked period".into(), at()), Ok(true));
        assert_eq!(p.submit(at()), Ok(true));
        assert_eq!(p.attempts, 2);
        assert_eq!(p.status, PostingStatus::Submitted);
    }

    #[test]
    fn resubmit_is_idempotent() {
        let mut p = posting();
        assert_eq!(p.submit(at()), Ok(true));
        assert_eq!(p.submit(at()), Ok(false));
        assert_eq!(p.attempts, 1);
    }

    #[test]
    fn manual_resolution_from_any_open_state() {
        let user = UserId::try_from(99).unwrap();

        let mut p1 = posting();
        assert_eq!(p1.resolve("note".into(), None, user, at()), Ok(true));

        let mut p2 = posting();
        p2.submit(at()).unwrap();
        assert_eq!(
            p2.resolve("note".into(), Some("ext".into()), user, at()),
            Ok(true)
        );

        let mut p3 = posting();
        p3.submit(at()).unwrap();
        p3.fail("reason".into(), at()).unwrap();
        assert_eq!(p3.resolve("note".into(), None, user, at()), Ok(true));

        let mut p4 = posting();
        p4.submit(at()).unwrap();
        p4.deliver("ext".into(), at()).unwrap();
        assert!(matches!(
            p4.resolve("note".into(), None, user, at()),
            Err(AccountingError::Conflict(_))
        ));
    }

    #[test]
    fn terminal_states_reject_further_changes() {
        let user = UserId::try_from(99).unwrap();
        let mut p = posting();
        p.resolve("note".into(), None, user, at()).unwrap();

        assert!(matches!(p.submit(at()), Err(AccountingError::Conflict(_))));
        assert!(matches!(
            p.fail("fail".into(), at()),
            Err(AccountingError::Conflict(_))
        ));
        assert!(matches!(
            p.deliver("ext".into(), at()),
            Err(AccountingError::Conflict(_))
        ));
    }

    #[test]
    fn text_limits() {
        let mut p = posting();
        p.submit(at()).unwrap();

        assert!(matches!(
            p.fail(" ".repeat(10), at()),
            Err(AccountingError::Invalid(_))
        ));
        assert!(matches!(
            p.fail("a".repeat(1025), at()),
            Err(AccountingError::Invalid(_))
        ));
        assert!(p.fail("a".repeat(1024), at()).is_ok());

        let user = UserId::try_from(99).unwrap();
        let mut p2 = posting();
        assert!(matches!(
            p2.resolve("  ".into(), None, user, at()),
            Err(AccountingError::Invalid(_))
        ));
        assert!(matches!(
            p2.resolve("a".repeat(1025), None, user, at()),
            Err(AccountingError::Invalid(_))
        ));
        assert!(p2.resolve("a".repeat(1024), None, user, at()).is_ok());
    }
}
