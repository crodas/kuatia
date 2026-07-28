//! Error types for storage implementations.

use std::error::Error;
use std::fmt;

/// Errors produced by [`Store`](crate::store::Store) implementations.
///
/// The store is a dumb instruction follower: writes report affected-row counts,
/// not semantic verdicts, so there are no "posting not active"/"reservation
/// mismatch"/"cas conflict"/"already exists"/"version conflict" variants — every
/// caller derives those from counts. The only outcomes a write can report are a
/// count and an I/O fault.
#[derive(Debug, Clone)]
pub enum StoreError {
    /// The requested entity was not found.
    NotFound(String),
    /// Catch-all for unexpected internal errors.
    Internal(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(msg) => write!(f, "not found: {msg}"),
            Self::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

impl Error for StoreError {}
