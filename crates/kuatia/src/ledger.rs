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
mod projection;
mod query;

pub use balance::SubAccountBalance;
pub use commit::LoadedState;

/// Default grace window (milliseconds) for the balance projection watermark: how
/// far behind live a snapshot is allowed to advance, covering commit-to-visibility
/// lag. See ADR-0019.
pub const DEFAULT_PROJECTION_GRACE_MS: i64 = 60_000;

/// Default number of credits/debits that must accrue for a `(account, asset)`
/// since its latest cache point before a read appends a new one. See ADR-0019.
pub const DEFAULT_SNAPSHOT_INTERVAL: u64 = 128;

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
    /// Grace window (ms) a cache point keeps behind live when its watermark is
    /// set (ADR-0019).
    projection_grace_ms: i64,
    /// Credits/debits that must accrue since the latest cache point before a read
    /// appends a new one (ADR-0019).
    snapshot_interval: u64,
}

impl Ledger {
    /// Create a new ledger backed by the given store.
    pub fn new(store: impl Store + 'static) -> Self {
        Self {
            store: Arc::new(store),
            projection_grace_ms: DEFAULT_PROJECTION_GRACE_MS,
            snapshot_interval: DEFAULT_SNAPSHOT_INTERVAL,
        }
    }

    /// Set the cache-point grace window (ms). Larger is safer against
    /// commit-to-visibility lag but keeps the read tail longer. See ADR-0019.
    pub fn with_projection_grace_ms(mut self, grace_ms: i64) -> Self {
        self.projection_grace_ms = grace_ms;
        self
    }

    /// Set how many credits/debits accrue before a read appends a new cache
    /// point. Smaller snapshots more often (shorter tails, more rows); larger
    /// snapshots less often. See ADR-0019.
    pub fn with_snapshot_interval(mut self, interval: u64) -> Self {
        self.snapshot_interval = interval;
        self
    }

    /// Returns a reference to the underlying store.
    pub fn store(&self) -> &dyn Store {
        self.store.as_ref()
    }
}
