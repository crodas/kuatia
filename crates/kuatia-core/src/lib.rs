//! Pure, sans-IO decision logic for the ledger.
//!
//! This crate contains no IO, no async runtime, and near-zero dependencies so that
//! the auditable heart of the ledger can be tested with golden vectors, replayed
//! deterministically, and embedded anywhere.

pub mod hash;
pub mod posting_resolution;
pub mod posting_selection;
pub mod validate;

pub use kuatia_types::*;

pub use hash::{
    account_canonical_bytes, account_hash, account_snapshot_id, canonical_bytes, content_hash,
    double_sha256, envelope_id,
};
pub use posting_resolution::{
    Debit, MovementDraft, ResolveError, ResolveInput, draft_movements, resolve_envelope,
};
pub use posting_selection::{SelectionError, select_postings};
pub use validate::{Plan, PlanInput, ValidationError, validate_and_plan};
