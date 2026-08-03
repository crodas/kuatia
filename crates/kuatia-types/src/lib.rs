//! Domain types for the ledger.
//!
//! These types model the UTXO-style ledger where value is held as **postings** —
//! signed amounts owned by exactly one account. An account's balance is simply the
//! sum of its active postings, which eliminates the need for running balance fields
//! and makes the system trivially auditable by replaying the transfer log.

pub mod autoid;

mod account;
mod account_code;
mod book;
mod canonical;
mod envelope;
mod ids;
mod posting;
mod transfer;

pub use account_code::{
    DEFAULT_ID_SEED, ID_BITS, ParseAccountIdError, SUB_BITS, id_seed, set_id_seed,
};

// The content-addressing contract (trait, version byte, write helpers, and
// every `impl ToBytes`) lives in `canonical`. Re-exported here so the public
// surface stays `kuatia_types::{ToBytes, CANONICAL_VERSION, write_*}`.
pub use canonical::{
    CANONICAL_VERSION, ToBytes, write_i64, write_u16, write_u32, write_u64, write_u128,
};

// Cent — re-exported from kuatia-money (swappable integer backing).
pub use kuatia_money::{Amount, Cent, OverflowError, ParseAmountError};

pub use account::{Account, AccountFlags};
pub use book::{Book, BookBuilder, BookPolicy};
pub use envelope::{Envelope, EnvelopeBuilder, Metadata};
pub use ids::{
    AccountId, AccountSnapshotId, AssetId, BookId, DEFAULT_BOOK, EnvelopeId, PostingId,
    ReservationId,
};
pub use posting::{NewPosting, Posting, PostingFilter, PostingState};
pub use transfer::{Movement, Receipt, Transfer, TransferBuilder};
