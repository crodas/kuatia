//! Posting records and their derived lifecycle state.

use crate::ids::{AccountId, AssetId, PostingId, ReservationId};
use kuatia_money::Cent;
use serde::{Deserialize, Serialize};

/// Read filter over the derived lifecycle state of postings.
///
/// A posting's state is no longer stored on the posting itself; it is derived
/// from index-table membership. This filter selects which postings a read
/// returns:
///
/// - `Active` — spendable (present in the active index).
/// - `Reserved` — claimed by an in-flight saga (present in the reserved index).
/// - `Live` — `Active ∪ Reserved`; everything that still counts toward balance.
/// - `All` — every posting in the immutable table, including spent ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PostingFilter {
    /// Spendable postings only.
    Active,
    /// Reserved (in-flight) postings only.
    Reserved,
    /// Active or reserved: the balance-bearing set (everything not yet Spent).
    Live,
    /// Every posting ever created, including spent ones.
    All,
}

/// The derived lifecycle state of a single [`Posting`], computed from
/// index-table membership rather than stored on the posting.
///
/// ```text
/// Active ──reserve──▶ Reserved(rid) ──consume──▶ Spent
///   ▲  ▲                   │
///   │  └── release ────────┘  (compensation)
///   └── (id in active index)
/// ```
///
/// `Reserved` carries the owning [`ReservationId`] so a saga can confirm it
/// still holds a posting before finalizing or releasing it. `Missing` means the
/// id is not present in the immutable postings table at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PostingState {
    /// Present in the active index — spendable, counts toward balance.
    Active,
    /// Present in the reserved index, claimed by the given reservation.
    Reserved(ReservationId),
    /// Present only in the immutable table — consumed by a committed transfer.
    Spent,
    /// Not present in the immutable table.
    Missing,
}

/// A signed amount of one asset, owned by exactly one account.
///
/// A positive posting is value controlled by the account; a negative posting is
/// an offset position (issuance, external flow, overdraft, or system balancing).
/// Negative postings are allowed on any account except one that forbids
/// overdraft (carries [`AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT`](crate::AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT)).
///
/// A `Posting` is an immutable record: once created it is never updated. Its
/// lifecycle state is not a field here; it is derived from index-table
/// membership (see [`PostingState`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Posting {
    /// Unique identifier derived from the creating transfer.
    pub id: PostingId,
    /// The account (subaccount) that owns this posting.
    pub owner: AccountId,
    /// The asset this posting denominates.
    pub asset: AssetId,
    /// Signed: positive = value controlled by the account, negative = offset position.
    pub value: Cent,
}

impl Posting {
    /// Construct a posting record.
    pub fn new(id: PostingId, owner: AccountId, asset: AssetId, value: Cent) -> Self {
        Self {
            id,
            owner,
            asset,
            value,
        }
    }
}

/// A posting to be created — carries no id yet because the [`PostingId`] depends
/// on the [`EnvelopeId`](crate::EnvelopeId), which is computed during validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewPosting {
    /// The account (subaccount) that will own the created posting.
    pub owner: AccountId,
    /// The asset this posting denominates.
    pub asset: AssetId,
    /// Signed amount: positive = value controlled by the account, negative = offset position.
    pub value: Cent,
    /// Provenance — who funded this posting. Descriptive only: it is excluded
    /// from the content-address preimage (see [`NewPosting`]'s `ToBytes`), so it
    /// does not affect the [`EnvelopeId`](crate::EnvelopeId) or the idempotency
    /// key. Persisted on the envelope record for audit, not on the stored
    /// [`Posting`].
    pub payer: Option<AccountId>,
}
