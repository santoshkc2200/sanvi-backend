//! Infrastructure-free cryptographic primitives shared by bounded contexts.
//!
//! Contexts keep their own key tables and stores (one schema per context);
//! only the byte formats and algorithms live here, so every context seals and
//! signs the same way.

pub mod envelope;
pub mod signed;

pub use envelope::{CryptoError, Key, generate_key, open, seal};
pub use signed::{SignedError, sign, verify};
