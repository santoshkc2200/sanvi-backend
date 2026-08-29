//! The pure chain verifier: entries in insertion order must form a single
//! hash chain starting at [`GENESIS_HASH`].

use crate::domain::{AuditEntry, AuditError, GENESIS_HASH};

/// Verify a whole chain (from genesis) held in memory at once.
pub fn verify_chain(entries: &[AuditEntry]) -> Result<(), AuditError> {
    match verify_segment(entries, GENESIS_HASH) {
        Ok(_) => Ok(()),
        Err(err) => Err(err),
    }
}

/// Verify one in-order segment that continues from `previous_hash`, and
/// return the segment's last hash so a caller can continue across pages.
///
/// Callers order by `seq` — the insertion order the writer chains with.
/// Every entry must hash over the previous entry's hash, and every entry
/// must verify against its own content.
pub fn verify_segment(
    entries: &[AuditEntry],
    previous_hash: [u8; 32],
) -> Result<[u8; 32], AuditError> {
    let mut running = previous_hash;
    for (position, entry) in entries.iter().enumerate() {
        if entry.prev_hash != running {
            return Err(AuditError::ChainMismatch {
                entry_id: entry.id,
                position,
                detail: "prev_hash does not match the previous entry".to_string(),
            });
        }
        if !entry.verify() {
            return Err(AuditError::ChainMismatch {
                entry_id: entry.id,
                position,
                detail: "entry hash does not match its content".to_string(),
            });
        }
        running = entry.hash;
    }
    Ok(running)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use std::sync::atomic::{AtomicI64, Ordering};

    use sanvi_kernel::SnowflakeId;

    use super::*;
    use crate::domain::{ActorType, AuditEntry, AuditEntryData};

    static NEXT_ID: AtomicI64 = AtomicI64::new(1);

    fn entry(prev: [u8; 32]) -> AuditEntry {
        let id = SnowflakeId::try_from(NEXT_ID.fetch_add(1, Ordering::Relaxed)).unwrap();
        AuditEntry::new(
            id,
            AuditEntryData {
                occurred_at: Utc::now(),
                actor_type: ActorType::System,
                actor_id: None,
                on_behalf_of: None,
                tenant_id: None,
                action: "test.action".to_string(),
                resource_type: None,
                resource_id: None,
                before: None,
                after: None,
                request_id: None,
                ip: None,
                user_agent: None,
            },
            prev,
        )
        .unwrap()
    }

    #[test]
    fn a_valid_chain_verifies() {
        let first = entry(GENESIS_HASH);
        let second = entry(first.hash);
        let third = entry(second.hash);
        assert!(verify_chain(&[first, second, third]).is_ok());
    }

    #[test]
    fn an_empty_chain_verifies() {
        assert!(verify_chain(&[]).is_ok());
        assert_eq!(verify_segment(&[], GENESIS_HASH).unwrap(), GENESIS_HASH);
    }

    #[test]
    fn a_chain_not_starting_at_genesis_fails() {
        let mut first = entry(GENESIS_HASH);
        first.prev_hash = [9u8; 32];
        assert!(matches!(
            verify_chain(&[first]),
            Err(AuditError::ChainMismatch { position: 0, .. })
        ));
    }

    #[test]
    fn a_tampered_row_breaks_the_chain() {
        let first = entry(GENESIS_HASH);
        let mut second = entry(first.hash);
        second.action = "test.tampered".to_string();
        let err = verify_chain(&[first, second]).unwrap_err();
        assert!(matches!(err, AuditError::ChainMismatch { position: 1, .. }));
    }

    #[test]
    fn a_gap_breaks_the_chain() {
        let first = entry(GENESIS_HASH);
        let second = entry(first.hash);
        let third = entry(GENESIS_HASH); // re-linked to genesis: a fork
        assert!(verify_chain(&[first, second, third]).is_err());
    }

    #[test]
    fn segments_continue_across_pages() {
        let first = entry(GENESIS_HASH);
        let second = entry(first.hash);
        let third = entry(second.hash);
        // Page 1: first two entries. Page 2: the third, continuing from
        // page 1's last hash.
        let last = verify_segment(&[first, second.clone()], GENESIS_HASH).unwrap();
        assert_eq!(last, second.hash);
        assert!(verify_segment(&[third], last).is_ok());
    }

    #[test]
    fn mismatch_identifies_the_entry() {
        let first = entry(GENESIS_HASH);
        let mut second = entry(first.hash);
        let second_id = second.id;
        second.hash = [1u8; 32];
        let err = verify_chain(&[first, second]).unwrap_err();
        match err {
            AuditError::ChainMismatch {
                entry_id, position, ..
            } => {
                assert_eq!(entry_id, second_id);
                assert_eq!(position, 1);
            }
            other => panic!("expected ChainMismatch, got {other:?}"),
        }
    }

    #[test]
    fn ids_are_positive_snowflakes() {
        let first = entry(GENESIS_HASH);
        assert!(first.id.as_i64() > 0);
    }
}
