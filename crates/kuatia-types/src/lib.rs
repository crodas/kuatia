//! Domain types for the ledger.
//!
//! These types model the UTXO-style ledger where value is held as **postings** —
//! signed amounts owned by exactly one account. An account's balance is simply the
//! sum of its active postings, which eliminates the need for running balance fields
//! and makes the system trivially auditable by replaying the transfer log.

pub mod autoid;

mod account_code;

pub use account_code::{
    DEFAULT_ID_SEED, ID_BITS, ParseAccountIdError, SUB_BITS, id_seed, set_id_seed,
};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

// ---------------------------------------------------------------------------
// ToBytes trait
// ---------------------------------------------------------------------------

/// Deterministic binary serialization. Every domain type can produce its
/// canonical byte representation.
pub trait ToBytes {
    /// Returns the canonical byte representation of this value.
    fn to_bytes(&self) -> Vec<u8>;
}

// ---------------------------------------------------------------------------
// Binary encoding helpers — big-endian, deterministic
// ---------------------------------------------------------------------------

/// Version byte prepended to canonical serializations for forward compatibility.
/// Bumped to 2 when `Cent` moved to a fixed 16-byte canonical encoding (ADR-0011).
/// Bumped to 3 when `AccountId` gained a `subaccount` leg folded into its
/// canonical bytes (ADR-0012).
/// Bumped to 4 when the vestigial `UserData` fields were removed from the
/// `Envelope` and `Account` preimages.
pub const CANONICAL_VERSION: u8 = 4;

/// Append a `u16` in big-endian to `buf`.
pub fn write_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_be_bytes());
}

/// Append a `u32` in big-endian to `buf`.
pub fn write_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_be_bytes());
}

/// Append a `u64` in big-endian to `buf`.
pub fn write_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_be_bytes());
}

/// Append an `i64` in big-endian to `buf`.
pub fn write_i64(buf: &mut Vec<u8>, v: i64) {
    buf.extend_from_slice(&v.to_be_bytes());
}

/// Append a `u128` in big-endian to `buf`.
pub fn write_u128(buf: &mut Vec<u8>, v: u128) {
    buf.extend_from_slice(&v.to_be_bytes());
}

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Stable account identity. Used in all public APIs.
///
/// An account is a base `id` plus a `subaccount`. `sub = 0` is the main account
/// (the default when subaccounts are not used); a non-zero `sub` is a
/// subaccount of the same base id. Each `(id, sub)` is a full account record
/// with its own policy and lifecycle. See ADR-0012 and ADR-0015.
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
    /// [`SUB_BITS`] bits, so a subaccount id must fit in that range to round-trip.
    pub sub: i64,
}

/// Pairs an [`AccountId`] with a snapshot hash — the double-SHA256 of the
/// account's state at a point in time. Stored on [`Transfer`] to record which
/// account versions a transfer was executed against. Internal type — the
/// public API uses [`AccountId`].
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

// ---------------------------------------------------------------------------
// Cent — re-exported from kuatia-money (swappable integer backing)
// ---------------------------------------------------------------------------

pub use kuatia_money::{Amount, Cent, OverflowError, ParseAmountError};

impl ToBytes for Cent {
    fn to_bytes(&self) -> Vec<u8> {
        self.to_canonical_bytes().to_vec()
    }
}

// ---------------------------------------------------------------------------
// Debug / Display impls for identifiers
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
        static GEN: crate::autoid::AutoId = crate::autoid::AutoId::new();
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

/// Identifies a book — a named scope for transfers.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BookId(pub i64);

impl fmt::Debug for BookId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BookId({})", self.0)
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
    /// stable across calls — use it when creating a new [`Book`], never for the
    /// implicit book of a transfer.
    pub fn generate() -> Self {
        // Process-global so the "process-unique" contract holds across threads;
        // a per-thread generator can repeat an id on another thread.
        static GEN: crate::autoid::AutoId = crate::autoid::AutoId::new();
        Self(GEN.next())
    }
}

/// Identifies a reservation — the owner token recorded in the reserved index
/// while a posting is claimed, so only the saga that reserved it may finalize
/// or release it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReservationId(pub i64);

impl fmt::Debug for ReservationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ReservationId({})", self.0)
    }
}

impl ReservationId {
    /// Create a `ReservationId` from an `i64`.
    pub const fn new(id: i64) -> Self {
        Self(id)
    }
}

impl Default for ReservationId {
    fn default() -> Self {
        // One process-global generator, not one per thread: its atomic counter
        // makes every reservation id unique across threads. A `thread_local`
        // generator lets two sagas on different threads mint the same id within
        // a millisecond, which collapses the reservation-ownership check and
        // allows a double-spend under concurrency.
        static GEN: crate::autoid::AutoId = crate::autoid::AutoId::new();
        Self(GEN.next())
    }
}

// ---------------------------------------------------------------------------
// Book
// ---------------------------------------------------------------------------

/// A Book is a transfer policy scope: it gates which accounts and assets may
/// participate in a transfer. It is **not** the chronological entry log (the
/// transfer log plays that role), and it does **not** partition balances —
/// balances are global; a Book only gates participation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Book {
    /// Stable identity for this book.
    pub id: BookId,
    /// Human-readable name.
    pub name: String,
    /// Participation rules for this book.
    pub policy: BookPolicy,
}

/// The participation rules for a [`Book`]. An empty field means "no restriction".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookPolicy {
    /// If non-empty, only these assets may appear in movements.
    pub allowed_assets: Vec<AssetId>,
    /// If non-empty, accounts with ANY of these flags may participate.
    pub allowed_flags: AccountFlags,
    /// If non-empty, these specific accounts may participate (in addition to flag matches).
    pub allowed_accounts: Vec<AccountId>,
}

/// Builder for constructing [`Book`] values.
pub struct BookBuilder {
    book: Book,
}

impl BookBuilder {
    /// Create a new book builder with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            book: Book {
                id: BookId::generate(),
                name: name.into(),
                policy: BookPolicy {
                    allowed_assets: Vec::new(),
                    allowed_flags: AccountFlags::empty(),
                    allowed_accounts: Vec::new(),
                },
            },
        }
    }

    /// Set the book id explicitly.
    pub fn id(mut self, id: BookId) -> Self {
        self.book.id = id;
        self
    }

    /// Add an allowed asset.
    pub fn allow_asset(mut self, asset: AssetId) -> Self {
        self.book.policy.allowed_assets.push(asset);
        self
    }

    /// Set allowed account flags — accounts with ANY of these flags may participate.
    pub fn allow_flags(mut self, flags: AccountFlags) -> Self {
        self.book.policy.allowed_flags = flags;
        self
    }

    /// Add a specific allowed account.
    pub fn allow_account(mut self, account: AccountId) -> Self {
        self.book.policy.allowed_accounts.push(account);
        self
    }

    /// Consume the builder and return the [`Book`].
    pub fn build(self) -> Book {
        self.book
    }
}

// ---------------------------------------------------------------------------
// Posting
// ---------------------------------------------------------------------------

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
    /// Active or reserved — the balance-bearing set (the old "not Inactive").
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
/// Negative postings are allowed on every policy except `NoOverdraft`.
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
/// on the [`EnvelopeId`], which is computed during validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewPosting {
    /// The account (subaccount) that will own the created posting.
    pub owner: AccountId,
    /// The asset this posting denominates.
    pub asset: AssetId,
    /// Signed amount: positive = value controlled by the account, negative = offset position.
    pub value: Cent,
    /// Informational provenance — who funded this posting.
    pub payer: Option<AccountId>,
}

// ---------------------------------------------------------------------------
// Transfer
// ---------------------------------------------------------------------------

/// Free-form key→value metadata.
pub type Metadata = BTreeMap<String, Vec<u8>>;

/// The unit of atomicity — all of its consumptions and creations apply together
/// or not at all. This is the resolved, internal form produced by the saga
/// pipeline from a [`Transfer`] intent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// Posting ids consumed (spent) by this envelope.
    pub consumes: Vec<PostingId>,
    /// New postings created by this envelope.
    pub creates: Vec<NewPosting>,
    /// Account version pins for optimistic concurrency.
    pub account_snapshots: Vec<AccountSnapshotId>,
    /// Book this envelope belongs to.
    pub book: BookId,
    /// Free-form key-value metadata.
    pub metadata: Metadata,
}

impl Envelope {
    /// Posting ids consumed (spent) by this envelope.
    pub fn consumes(&self) -> &[PostingId] {
        &self.consumes
    }

    /// New postings created by this envelope.
    pub fn creates(&self) -> &[NewPosting] {
        &self.creates
    }

    /// Account version pins for optimistic concurrency.
    pub fn account_snapshots(&self) -> &[AccountSnapshotId] {
        &self.account_snapshots
    }

    /// Book this envelope belongs to.
    pub fn book(&self) -> BookId {
        self.book
    }

    /// Free-form key-value metadata.
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// Deduplicated, sorted list of account references in the created postings.
    pub fn referenced_accounts(&self) -> Vec<AccountId> {
        let mut ids: Vec<AccountId> = self.creates.iter().map(|p| p.owner).collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Set account snapshots.
    pub fn set_account_snapshots(&mut self, snapshots: Vec<AccountSnapshotId>) {
        self.account_snapshots = snapshots;
    }
}

// ---------------------------------------------------------------------------
// EnvelopeBuilder
// ---------------------------------------------------------------------------

/// Builder for constructing [`Envelope`] values.
#[derive(Default)]
pub struct EnvelopeBuilder {
    envelope: Envelope,
}

impl EnvelopeBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the posting ids to consume.
    pub fn consumes(mut self, ids: Vec<PostingId>) -> Self {
        self.envelope.consumes = ids;
        self
    }

    /// Set the new postings to create.
    pub fn creates(mut self, postings: Vec<NewPosting>) -> Self {
        self.envelope.creates = postings;
        self
    }

    /// Set the book.
    pub fn book(mut self, book: BookId) -> Self {
        self.envelope.book = book;
        self
    }

    /// Set the account version pins.
    pub fn account_snapshots(mut self, snapshots: Vec<AccountSnapshotId>) -> Self {
        self.envelope.account_snapshots = snapshots;
        self
    }

    /// Set the free-form metadata.
    pub fn metadata(mut self, metadata: Metadata) -> Self {
        self.envelope.metadata = metadata;
        self
    }

    /// Consume the builder and return the [`Envelope`].
    pub fn build(self) -> Envelope {
        self.envelope
    }
}

// ---------------------------------------------------------------------------
// Account
// ---------------------------------------------------------------------------

/// Controls how much an account can spend beyond its posting-backed balance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccountPolicy {
    /// Balance must stay >= 0.
    NoOverdraft,
    /// Balance must stay >= `floor` (floor < 0).
    CappedOverdraft {
        /// Minimum allowed balance (must be negative).
        floor: Cent,
    },
    /// No floor — the account can go arbitrarily negative.
    UncappedOverdraft,
    /// Fees, settlement, market-making, minting. No balance constraints.
    SystemAccount,
    /// Boundary account representing value entering/leaving the ledger; holds
    /// the offset (negative) side of deposits.
    ExternalAccount,
}

bitflags::bitflags! {
    /// Lifecycle and user-defined flags for an [`Account`].
    ///
    /// Bits 0–7 are reserved for system flags. Bits 8–31 are available for
    /// user-defined flags, which can be used with [`BookPolicy::allowed_flags`]
    /// to scope which accounts may participate in a book.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct AccountFlags: u32 {
        /// Account may not be the source or destination of any transfer.
        const FROZEN = 1 << 0;
        /// Terminal — no further activity.
        const CLOSED = 1 << 1;
        /// Holding account for an inflight (authorize/confirm/void) transaction.
        /// Parks funds between authorize and settlement; closed once drained.
        const INFLIGHT = 1 << 2;
        // Bits 3–7: reserved for future system flags.
        // Bits 8–31: user-defined.
        /// User-defined flag 0.
        const USER_0 = 1 << 8;
        /// User-defined flag 1.
        const USER_1 = 1 << 9;
        /// User-defined flag 2.
        const USER_2 = 1 << 10;
        /// User-defined flag 3.
        const USER_3 = 1 << 11;
        /// User-defined flag 4.
        const USER_4 = 1 << 12;
        /// User-defined flag 5.
        const USER_5 = 1 << 13;
        /// User-defined flag 6.
        const USER_6 = 1 << 14;
        /// User-defined flag 7.
        const USER_7 = 1 << 15;
    }
}

/// A registered entity that must exist before it can transact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Stable identity for this account (base account plus subaccount).
    pub id: AccountId,
    /// Monotonically increasing version, starts at 1 on creation.
    pub version: u64,
    /// Overdraft / balance policy.
    pub policy: AccountPolicy,
    /// Lifecycle flags (frozen, closed).
    pub flags: AccountFlags,
    /// Book this entity belongs to.
    pub book: BookId,
    /// Free-form key-value metadata.
    pub metadata: Metadata,
}

impl Account {
    /// Create a version-1 main-subaccount account with the given policy: no flags,
    /// the default book, and empty metadata. Convenience for the common case; set
    /// the other fields explicitly when you need them.
    pub fn new(id: AccountId, policy: AccountPolicy) -> Self {
        Self::new_ref(id, policy)
    }

    /// Like [`Account::new`] but for a specific subaccount reference.
    pub fn new_ref(id: AccountId, policy: AccountPolicy) -> Self {
        Self {
            id,
            version: 1,
            policy,
            flags: AccountFlags::empty(),
            book: DEFAULT_BOOK,
            metadata: Metadata::new(),
        }
    }

    /// Returns `true` if the account has the `FROZEN` flag set.
    pub fn is_frozen(&self) -> bool {
        self.flags.contains(AccountFlags::FROZEN)
    }

    /// Returns `true` if the account has the `CLOSED` flag set.
    pub fn is_closed(&self) -> bool {
        self.flags.contains(AccountFlags::CLOSED)
    }
}

// ---------------------------------------------------------------------------
// Receipt
// ---------------------------------------------------------------------------

/// Confirmation of a committed transfer, carrying its content-addressed id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    /// Content-addressed id of the committed transfer.
    pub transfer_id: EnvelopeId,
}

// ---------------------------------------------------------------------------
// Transfer — intent-based API
// ---------------------------------------------------------------------------

/// A single movement within a transfer: move value from one account to another.
///
/// Every operation (pay, deposit, withdraw) is expressed as one or more
/// movements.  The resolve step aggregates net debits per account and selects
/// postings only for accounts with a positive net debit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Movement {
    /// Account (subaccount) being debited.
    pub from: AccountId,
    /// Account (subaccount) being credited.
    pub to: AccountId,
    /// Asset to transfer.
    pub asset: AssetId,
    /// Amount to transfer (may be negative for offset postings).
    pub amount: Cent,
}

/// A transfer intent — one or more movements to execute atomically.
///
/// The saga pipeline resolves movements into concrete postings ([`Envelope`])
/// during execution. Callers express *what* should happen, not *which postings*
/// to consume.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transfer {
    /// Movements to execute atomically.
    pub movements: Vec<Movement>,
    /// Book this entity belongs to.
    pub book: BookId,
    /// Free-form key-value metadata.
    pub metadata: Metadata,
}

/// Builder for constructing [`Transfer`] values.
#[derive(Default)]
pub struct TransferBuilder {
    transfer: Transfer,
}

impl TransferBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a raw movement between main subaccounts.
    pub fn movement(self, from: AccountId, to: AccountId, asset: AssetId, amount: Cent) -> Self {
        self.movement_ref(from, to, asset, amount)
    }

    /// Add a raw movement between specific subaccounts.
    pub fn movement_ref(
        mut self,
        from: AccountId,
        to: AccountId,
        asset: AssetId,
        amount: Cent,
    ) -> Self {
        self.transfer.movements.push(Movement {
            from,
            to,
            asset,
            amount,
        });
        self
    }

    /// Add a pay movement between main subaccounts.
    pub fn pay(self, from: AccountId, to: AccountId, asset: AssetId, amount: Cent) -> Self {
        self.movement(from, to, asset, amount)
    }

    /// Add a pay movement between two specific subaccounts. See
    /// [`movement_ref`](Self::movement_ref).
    pub fn pay_ref(self, from: AccountId, to: AccountId, asset: AssetId, amount: Cent) -> Self {
        self.movement_ref(from, to, asset, amount)
    }

    /// Add a deposit: creates an offset posting on the external account and
    /// credits the target account.  Pushes two movements whose net debit on the
    /// external account is zero.
    pub fn deposit(
        self,
        to: AccountId,
        asset: AssetId,
        amount: Cent,
        external: AccountId,
    ) -> Result<Self, OverflowError> {
        let neg = amount.checked_neg()?;
        Ok(self
            .movement(external, external, asset, neg)
            .movement(external, to, asset, amount))
    }

    /// Add a withdrawal: move value from an account to an external destination.
    pub fn withdraw(
        self,
        from: AccountId,
        asset: AssetId,
        amount: Cent,
        external: AccountId,
    ) -> Self {
        self.movement(from, external, asset, amount)
    }

    /// Set the book.
    pub fn book(mut self, book: BookId) -> Self {
        self.transfer.book = book;
        self
    }

    /// Set the free-form metadata.
    pub fn metadata(mut self, metadata: Metadata) -> Self {
        self.transfer.metadata = metadata;
        self
    }

    /// Consume the builder and return the [`Transfer`].
    pub fn build(self) -> Transfer {
        self.transfer
    }
}

// ---------------------------------------------------------------------------
// ToBytes implementations
// ---------------------------------------------------------------------------

impl ToBytes for AccountId {
    fn to_bytes(&self) -> Vec<u8> {
        // Base id then subaccount, both big-endian, so the subaccount is folded
        // into every content hash (envelope ids, posting ids, snapshots).
        let mut buf = Vec::with_capacity(16);
        buf.extend_from_slice(&self.id.to_be_bytes());
        buf.extend_from_slice(&self.sub.to_be_bytes());
        buf
    }
}

impl ToBytes for AccountSnapshotId {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(48);
        buf.extend_from_slice(&self.account.to_bytes());
        buf.extend_from_slice(&self.snapshot_id);
        buf
    }
}

impl ToBytes for AssetId {
    fn to_bytes(&self) -> Vec<u8> {
        self.0.to_be_bytes().to_vec()
    }
}

impl ToBytes for EnvelopeId {
    fn to_bytes(&self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl ToBytes for PostingId {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(34);
        buf.extend_from_slice(&self.transfer.0);
        write_u16(&mut buf, self.index);
        buf
    }
}

impl ToBytes for AccountPolicy {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(9);
        match self {
            Self::NoOverdraft => buf.push(0),
            Self::CappedOverdraft { floor } => {
                buf.push(1);
                buf.extend(floor.to_bytes());
            }
            Self::UncappedOverdraft => buf.push(2),
            Self::SystemAccount => buf.push(3),
            Self::ExternalAccount => buf.push(4),
        }
        buf
    }
}

impl ToBytes for AccountFlags {
    fn to_bytes(&self) -> Vec<u8> {
        self.bits().to_be_bytes().to_vec()
    }
}

impl ToBytes for BookId {
    fn to_bytes(&self) -> Vec<u8> {
        self.0.to_be_bytes().to_vec()
    }
}

impl ToBytes for NewPosting {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend(self.owner.to_bytes());
        buf.extend_from_slice(&self.asset.0.to_be_bytes());
        buf.extend(self.value.to_bytes());
        match &self.payer {
            Some(p) => {
                buf.push(1);
                buf.extend(p.to_bytes());
            }
            None => buf.push(0),
        }
        buf
    }
}

impl ToBytes for Posting {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend(self.id.to_bytes());
        buf.extend(self.owner.to_bytes());
        buf.extend_from_slice(&self.asset.0.to_be_bytes());
        buf.extend(self.value.to_bytes());
        buf
    }
}

impl ToBytes for Envelope {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(CANONICAL_VERSION);

        write_u32(&mut buf, self.consumes.len() as u32);
        for pid in &self.consumes {
            buf.extend(pid.to_bytes());
        }

        write_u32(&mut buf, self.creates.len() as u32);
        for np in &self.creates {
            buf.extend(np.to_bytes());
        }

        write_u32(&mut buf, self.account_snapshots.len() as u32);
        for snap in &self.account_snapshots {
            buf.extend(snap.to_bytes());
        }

        buf.extend(self.book.to_bytes());

        write_u32(&mut buf, self.metadata.len() as u32);
        for (key, value) in &self.metadata {
            let key_bytes = key.as_bytes();
            write_u32(&mut buf, key_bytes.len() as u32);
            buf.extend_from_slice(key_bytes);
            write_u32(&mut buf, value.len() as u32);
            buf.extend_from_slice(value);
        }

        buf
    }
}

impl ToBytes for Account {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(CANONICAL_VERSION);
        buf.extend(self.id.to_bytes());
        write_u64(&mut buf, self.version);
        buf.extend(self.policy.to_bytes());
        buf.extend(self.flags.to_bytes());
        buf.extend(self.book.to_bytes());

        write_u32(&mut buf, self.metadata.len() as u32);
        for (key, value) in &self.metadata {
            let key_bytes = key.as_bytes();
            write_u32(&mut buf, key_bytes.len() as u32);
            buf.extend_from_slice(key_bytes);
            write_u32(&mut buf, value.len() as u32);
            buf.extend_from_slice(value);
        }

        buf
    }
}

impl ToBytes for Receipt {
    fn to_bytes(&self) -> Vec<u8> {
        self.transfer_id.0.to_vec()
    }
}
