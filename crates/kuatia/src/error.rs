//! Error types for the async ledger layer.
//!
//! [`LedgerError`] unifies errors from the pure core (validation, selection)
//! and from storage, so callers get a single error type from every API.

use kuatia_core::{
    AccountId, BookId, EnvelopeId, OverflowError, PostingId, SelectionError, ValidationError,
};
use kuatia_storage::error::StoreError;

/// Unified error type for the async ledger API.
#[derive(Debug)]
pub enum LedgerError {
    /// A transfer invariant was violated.
    Validation(ValidationError),
    /// Storage operation failed.
    Store(StoreError),
    /// Posting selection failed (e.g. insufficient funds).
    Selection(SelectionError),
    /// The referenced transfer does not exist.
    TransferNotFound(EnvelopeId),
    /// The posting cannot be reversed (e.g. already consumed).
    PostingNotReversible(PostingId),
    /// The referenced account does not exist.
    AccountNotFound(AccountId),
    /// Cannot close an account that still has active postings.
    AccountNotEmpty(AccountId),
    /// The account is already closed.
    AccountAlreadyClosed(AccountId),
    /// A transfer named a book that does not exist.
    BookNotFound(BookId),
    /// Monetary arithmetic overflow.
    Overflow,
    /// A saga step failed and its compensation also failed.
    CompensationFailed {
        /// The error that triggered compensation.
        original: Box<LedgerError>,
        /// The error that occurred during compensation.
        compensation: Box<LedgerError>,
    },
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(e) => write!(f, "validation: {e}"),
            Self::Store(e) => write!(f, "store: {e}"),
            Self::Selection(e) => write!(f, "selection: {e}"),
            Self::TransferNotFound(id) => write!(f, "transfer not found: {id:?}"),
            Self::PostingNotReversible(id) => write!(f, "posting not reversible: {id:?}"),
            Self::AccountNotFound(id) => write!(f, "account not found: {id:?}"),
            Self::AccountNotEmpty(id) => write!(f, "account not empty: {id:?}"),
            Self::AccountAlreadyClosed(id) => write!(f, "account already closed: {id:?}"),
            Self::BookNotFound(id) => write!(f, "book not found: {id:?}"),
            Self::Overflow => write!(f, "monetary amount overflow"),
            Self::CompensationFailed {
                original,
                compensation,
            } => write!(
                f,
                "compensation failed: original={original}, compensation={compensation}"
            ),
        }
    }
}

impl std::error::Error for LedgerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Validation(e) => Some(e),
            Self::Store(e) => Some(e),
            Self::Selection(e) => Some(e),
            Self::Overflow => Some(&OverflowError),
            Self::CompensationFailed { original, .. } => Some(original.as_ref()),
            _ => None,
        }
    }
}

impl From<ValidationError> for LedgerError {
    fn from(e: ValidationError) -> Self {
        LedgerError::Validation(e)
    }
}

impl From<StoreError> for LedgerError {
    fn from(e: StoreError) -> Self {
        LedgerError::Store(e)
    }
}

impl From<SelectionError> for LedgerError {
    fn from(e: SelectionError) -> Self {
        LedgerError::Selection(e)
    }
}

impl From<OverflowError> for LedgerError {
    fn from(_: OverflowError) -> Self {
        LedgerError::Overflow
    }
}
