//! Shared error mapping for transaction/commit failures in this context.

use crate::domain::BillingError;

/// Map a `TxManager` begin/commit failure into the domain's storage error.
pub fn storage(err: anyhow::Error) -> BillingError {
    BillingError::Storage(err.to_string())
}
