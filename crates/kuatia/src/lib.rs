//! Kuatia — async ledger resource built on top of [`kuatia_core`].
//!
//! This crate adds IO to the pure decision logic: the [`Store`](kuatia_storage::store::Store) trait
//! abstracts storage, and the [`Ledger`](crate::ledger::Ledger) struct drives the two-step
//! commit saga (reserve then finalize, validation inside finalize) behind an async API.

pub mod error;
pub mod inflight;
pub mod ledger;
pub mod saga;

// Re-export storage crate for convenience.
pub use kuatia_storage::{error as store_error, mem_store, store, store_tests};

pub mod prelude;
