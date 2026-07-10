//! Common imports for building on the ledger.
//!
//! `use kuatia::prelude::*;` brings the domain types and intent builders from
//! [`kuatia_core`] into scope, along with the [`Ledger`] resource, the
//! [`Store`] trait, and the [`InMemoryStore`]. Reach for the individual crates
//! when you need types the prelude does not surface.

pub use kuatia_core::*;

pub use crate::error::LedgerError;
pub use crate::inflight::{
    Authorization, InflightLeg, InflightLegStatus, InflightState, InflightStatus,
};
pub use crate::ledger::Ledger;
pub use kuatia_storage::mem_store::InMemoryStore;
pub use kuatia_storage::store::Store;
