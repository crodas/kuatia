//! Error types for storage implementations.

use kuatia_types::{AccountId, AssetId, PostingId};

/// Errors produced by [`Store`](crate::store::Store) implementations.
#[derive(Debug)]
pub enum StoreError {
    /// The requested entity was not found.
    NotFound(String),
    /// The entity already exists (e.g. duplicate account creation).
    AlreadyExists(String),
    /// Optimistic version check failed on an account update.
    VersionConflict {
        /// Account that had a version mismatch.
        account: AccountId,
        /// Version the caller expected.
        expected: u64,
        /// Version the store actually had.
        actual: u64,
    },
    /// Catch-all for unexpected internal errors.
    Internal(String),
    /// Attempted to reserve a posting that is not Active.
    PostingNotActive(PostingId),
    /// Attempted to release a void (Inactive) posting.
    PostingInactive(PostingId),
    /// A CAS guard's expected balance no longer matches the current balance —
    /// a concurrent transfer moved it. The caller should re-validate and retry.
    Conflict {
        /// Account whose balance changed under the guard.
        account: AccountId,
        /// Asset whose balance changed.
        asset: AssetId,
    },
    /// A posting's reservation owner did not match the caller's reservation —
    /// it is reserved by a different saga (or not reserved at all).
    ReservationMismatch(PostingId),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(msg) => write!(f, "not found: {msg}"),
            Self::AlreadyExists(msg) => write!(f, "already exists: {msg}"),
            Self::VersionConflict {
                account,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "version conflict for {account:?}: expected {expected}, got {actual}"
                )
            }
            Self::Internal(msg) => write!(f, "internal error: {msg}"),
            Self::PostingNotActive(id) => write!(f, "posting not active: {id:?}"),
            Self::PostingInactive(id) => write!(f, "posting is void (inactive): {id:?}"),
            Self::Conflict { account, asset } => {
                write!(f, "cas conflict on {account:?}/{asset:?}: balance changed")
            }
            Self::ReservationMismatch(id) => write!(f, "reservation mismatch on posting {id:?}"),
        }
    }
}

impl std::error::Error for StoreError {}
