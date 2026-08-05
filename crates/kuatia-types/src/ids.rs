//! Identifier newtypes for the ledger domain.
//!
//! Each id is a thin wrapper over an integer or byte array with its own
//! `Debug`, constructors, and (where minted) a snowflake-backed `Default`.

use crate::autoid::AutoId;
use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Stable account identity. Used in all public APIs.
///
/// An account is a base `id` plus a `subaccount`. `sub = 0` is the main account
/// (the default when subaccounts are not used); a non-zero `sub` is a
/// subaccount of the same base id. Each `(id, sub)` is a full account record
/// with its own flags and lifecycle. See ADR-0012 and ADR-0015.
///
/// Both legs are stored as `i64` (they hash and persist as full `i64`), but the
/// IBAN-style string form ([`Display`](fmt::Display) / [`FromStr`](std::str::FromStr))
/// encodes only the low `ID_BITS` of `id` (a 63-bit snowflake never sets the
/// sign bit) and the low `SUB_BITS` of `sub`. That is what lets the code fit in
/// a fixed 20 characters. Values outside those ranges still hash, persist, and
/// compare correctly, but do not round-trip through the string form.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AccountId {
    /// Base account id (a 63-bit snowflake; the sign bit is always 0).
    pub id: i64,
    /// Subaccount id; `0` is the main account. The string form encodes the low
    /// [`SUB_BITS`](crate::SUB_BITS) bits, so a subaccount id must fit in that
    /// range to round-trip.
    pub sub: i64,
}

/// Pairs an [`AccountId`] with a snapshot hash — the double-SHA256 of the
/// account's state at a point in time. Stored on [`Transfer`](crate::Transfer)
/// to record which account versions a transfer was executed against. Internal
/// type — the public API uses [`AccountId`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountSnapshotId {
    /// The account (subaccount) this snapshot belongs to.
    pub account: AccountId,
    /// Double-SHA256 of the account's state at the time of the snapshot.
    pub snapshot_id: [u8; 32],
}

/// Identifies an asset (USD, EUR, BTC, …). Conservation is enforced per asset,
/// so each asset is an independent conservation boundary.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AssetId(pub u32);

/// Content-addressed transfer identifier — the double-SHA256 of the canonical
/// serialization. This makes the id both the idempotency key and the
/// tamper-evidence artifact.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EnvelopeId(pub [u8; 32]);

/// Uniquely identifies a posting within the ledger. The `(transfer, index)` pair
/// ties every posting back to the transfer that created it, which is the basis
/// of the provenance graph.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PostingId {
    /// The transfer that created this posting.
    pub transfer: EnvelopeId,
    /// Zero-based position within the transfer's created postings.
    pub index: u16,
}

/// Identifies a book — a named scope for transfers.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BookId(pub i64);

/// Identifies a reservation — the owner token recorded in the reserved index
/// while a posting is claimed, so only the saga that reserved it may finalize
/// or release it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReservationId(pub i64);

/// Identifies an account-version transition's write-ahead record. A distinct
/// type from [`ReservationId`] so a transition does not masquerade as a
/// reservation, but drawn from the same generator (see [`ReservationId::default`])
/// so the two write-ahead kinds share one collision-free id space.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TransitionId(pub i64);

// ---------------------------------------------------------------------------
// Debug impls for identifiers
// ---------------------------------------------------------------------------

impl fmt::Debug for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.sub == 0 {
            write!(f, "AccountId({})", self.id)
        } else {
            write!(f, "AccountId({}.{})", self.id, self.sub)
        }
    }
}

impl fmt::Debug for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AssetId({:#010x})", self.0)
    }
}

impl fmt::Debug for EnvelopeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EnvelopeId({})", hex(&self.0))
    }
}

impl fmt::Debug for PostingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostingId")
            .field("transfer", &self.transfer)
            .field("index", &self.index)
            .finish()
    }
}

impl fmt::Debug for BookId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BookId({})", self.0)
    }
}

impl fmt::Debug for ReservationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ReservationId({})", self.0)
    }
}

impl fmt::Debug for TransitionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TransitionId({})", self.0)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Identifier constructors
// ---------------------------------------------------------------------------

impl Default for AccountId {
    fn default() -> Self {
        // Process-global generator: a per-thread one could mint the same id on
        // two threads within a millisecond, yielding duplicate account ids.
        static GEN: AutoId = AutoId::new();
        Self {
            id: GEN.next(),
            sub: 0,
        }
    }
}

impl AccountId {
    /// Create the main account (`sub = 0`) for a base `id`.
    pub const fn new(id: i64) -> Self {
        Self { id, sub: 0 }
    }

    /// Create a specific subaccount of a base `id`.
    pub const fn with_sub(id: i64, sub: i64) -> Self {
        Self { id, sub }
    }

    /// Return the main account of this id (`sub` set to `0`).
    pub const fn base(&self) -> Self {
        Self {
            id: self.id,
            sub: 0,
        }
    }

    /// Whether this is the main account (`sub == 0`).
    pub const fn is_main(&self) -> bool {
        self.sub == 0
    }
}

impl From<AccountSnapshotId> for AccountId {
    fn from(snap: AccountSnapshotId) -> Self {
        snap.account
    }
}

impl AssetId {
    /// Create an `AssetId` from a `u32`.
    pub const fn new(id: u32) -> Self {
        Self(id)
    }
}

/// The implicit book used when a transfer does not name one. Fixed so that two
/// otherwise-identical transfers hash to the same [`EnvelopeId`] — a random
/// default would break content-addressed idempotency.
pub const DEFAULT_BOOK: BookId = BookId(0);

impl Default for BookId {
    /// Deterministic: returns [`DEFAULT_BOOK`]. Use [`BookId::generate`] to mint
    /// a fresh unique id for a real book.
    fn default() -> Self {
        DEFAULT_BOOK
    }
}

impl BookId {
    /// Create a `BookId` from an `i64`.
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    /// Mint a fresh, process-unique book id. Unlike [`Default`], this is not
    /// stable across calls — use it when creating a new [`Book`](crate::Book),
    /// never for the implicit book of a transfer.
    pub fn generate() -> Self {
        // Process-global so the "process-unique" contract holds across threads;
        // a per-thread generator can repeat an id on another thread.
        static GEN: AutoId = AutoId::new();
        Self(GEN.next())
    }
}

impl ReservationId {
    /// Create a `ReservationId` from an `i64`.
    pub const fn new(id: i64) -> Self {
        Self(id)
    }
}

/// The one process-global id source behind every write-ahead saga key —
/// reservation ids and transition ids alike. One atomic counter, not one per
/// thread: a `thread_local` generator lets two sagas on different threads mint
/// the same id within a millisecond, which collapses the reservation-ownership
/// check and allows a double-spend under concurrency. Sharing it across both
/// kinds is also what keeps their keys unique in the single saga keyspace.
fn next_saga_id() -> i64 {
    static GEN: AutoId = AutoId::new();
    GEN.next()
}

impl Default for ReservationId {
    fn default() -> Self {
        Self(next_saga_id())
    }
}

impl TransitionId {
    /// Create a `TransitionId` from an `i64`.
    pub const fn new(id: i64) -> Self {
        Self(id)
    }
}

impl Default for TransitionId {
    fn default() -> Self {
        Self(next_saga_id())
    }
}
