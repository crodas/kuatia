//! The error raised when an intent cannot be covered by the available postings.
//!
//! When a caller uses `pay` or `withdraw`, they specify an amount — not which
//! postings to consume. Resolution ([`crate::posting_resolution::resolve_envelope`])
//! picks the postings; when a non-overdraft account cannot cover the requested
//! amount it fails with [`SelectionError::InsufficientFunds`].

use kuatia_types::Cent;

/// Error returned when an amount cannot be covered by the available postings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionError {
    /// Available postings do not cover the requested amount.
    InsufficientFunds {
        /// Total value of eligible postings.
        available: Cent,
        /// Amount the caller asked for.
        requested: Cent,
    },
}

impl std::fmt::Display for SelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InsufficientFunds {
                available,
                requested,
            } => {
                write!(
                    f,
                    "insufficient funds: available {available}, requested {requested}"
                )
            }
        }
    }
}

impl std::error::Error for SelectionError {}
