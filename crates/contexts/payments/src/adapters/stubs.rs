//! Stub/sandbox adapters.
//!
//! `EmptyProviderCatalog` is 09.0's degenerate catalog (nothing
//! registered, nothing listed). 09.1 adds the fake provider adapter (the
//! proof the provider abstraction holds — the frontend builds 09.2–09.6
//! screens against it before any developer has Stripe sandbox
//! credentials wired); 09.2 adds the live Stripe Connect adapter behind
//! the same ports.

pub use crate::application::catalog::EmptyProviderCatalog;
