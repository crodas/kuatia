//! The async ledger resource -- the primary entry point for callers.
//!
//! [`Ledger`] is one type over a single [`Store`]. Its methods are grouped into
//! sibling modules by concern, so the deep commit engine is not buried among
//! shallow query re-exports:
//!
//! - `commit`: the write-ahead saga/commit engine (resolve, commit, reverse,
//!   recover, finalize). This is what a ledger fundamentally *is*.
//! - `lifecycle`: account create, freeze, unfreeze, close.
//! - `balance`: per-subaccount balance queries.
//! - `query`: read-only queries and book CRUD (thin `Store` pass-throughs that
//!   relabel `StoreError` as [`LedgerError`]).
//!
//! The inflight-hold API is a further cluster, in [`crate::inflight`].

use std::sync::Arc;

use kuatia_storage::store::Store;

// Kept in root scope so `envelope_saga`'s `use super::*` resolves the `legend!`
// macro and the types its expansion names.
use crate::error::LedgerError;
use crate::saga::{FinalizeTransferStep, LedgerCtx, ReservePostingsStep};
use legend::legend;

#[allow(missing_docs)]
mod envelope_saga;

mod balance;
mod commit;
mod lifecycle;
mod query;

pub use balance::SubAccountBalance;
pub use commit::LoadedState;

/// Return the current time as Unix milliseconds.
pub(crate) fn now_millis() -> Result<i64, LedgerError> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| LedgerError::Overflow)?
        .as_millis() as i64)
}

/// Async ledger resource composing the commit pipeline.
pub struct Ledger {
    store: Arc<dyn Store>,
}

impl Ledger {
    /// Create a new ledger backed by the given store.
    pub fn new(store: impl Store + 'static) -> Self {
        Self {
            store: Arc::new(store),
        }
    }

    /// Returns a reference to the underlying store.
    pub fn store(&self) -> &dyn Store {
        self.store.as_ref()
    }
}
