//! Error types for storage implementations.

use kuatia_types::AccountId;

/// Errors produced by [`Store`](crate::store::Store) implementations.
///
/// The store is a dumb instruction follower: writes report affected-row counts,
/// not semantic verdicts, so there are no "posting not active"/"reservation
/// mismatch"/"cas conflict" variants — the saga derives those from counts.
#[derive(Debug, Clone)]
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
        }
    }
}

impl std::error::Error for StoreError {}
