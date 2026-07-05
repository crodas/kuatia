//! Kuatia — async ledger resource built on top of [`kuatia_core`].
//!
//! This crate adds IO to the pure decision logic: the [`Store`](kuatia_storage::store::Store) trait
//! abstracts storage, and the [`Ledger`](crate::ledger::Ledger) struct composes the three-phase
//! commit pipeline (load → plan → apply) behind a convenient async API.

pub mod error;
pub mod inflight;
pub mod ledger;
pub mod saga;

// Re-export storage crate for convenience.
pub use kuatia_storage::{error as store_error, mem_store, store, store_tests};

/// Common imports for building on the ledger.
///
/// `use kuatia::prelude::*;` brings the domain types and intent builders from
/// [`kuatia_core`] into scope, along with the [`Ledger`](crate::ledger::Ledger)
/// resource, the [`Store`](kuatia_storage::store::Store) trait, and the
/// [`InMemoryStore`](kuatia_storage::mem_store::InMemoryStore). Reach for the
/// individual crates when you need types the prelude does not surface.
pub mod prelude {
    pub use kuatia_core::*;

    pub use crate::error::LedgerError;
    pub use crate::inflight::{
        Authorization, InflightLeg, InflightLegStatus, InflightState, InflightStatus,
    };
    pub use crate::ledger::Ledger;
    pub use kuatia_storage::mem_store::InMemoryStore;
    pub use kuatia_storage::store::Store;
}
