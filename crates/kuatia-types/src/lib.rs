//! Domain types for the ledger.
//!
//! These types model the UTXO-style ledger where value is held as **postings** —
//! signed amounts owned by exactly one account. An account's balance is simply the
//! sum of its active postings, which eliminates the need for running balance fields
//! and makes the system trivially auditable by replaying the transfer log.

pub mod autoid;

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
pub const CANONICAL_VERSION: u8 = 1;

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
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AccountId(pub i64);

/// Pairs an [`AccountId`] with a snapshot hash — the double-SHA256 of the
/// account's state at a point in time. Stored on [`Transfer`] to record which
/// account versions a transfer was executed against. Internal type — the
/// public API uses [`AccountId`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountSnapshotId {
    /// The account this snapshot belongs to.
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
// Cent — low-level stored monetary amount
// ---------------------------------------------------------------------------

/// A monetary amount in the smallest unit (e.g. cents for USD).
///
/// Wraps `i64` with a private field so that monetary values are never confused
/// with plain integers. Used everywhere a monetary amount is stored or compared.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct Cent(i64);

/// Returned when a [`Cent`] arithmetic operation would overflow or underflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverflowError;

impl fmt::Display for OverflowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "monetary amount overflow")
    }
}

impl std::error::Error for OverflowError {}

impl Cent {
    /// The zero amount.
    pub const ZERO: Cent = Cent(0);

    /// Returns the underlying `i64` value.
    pub fn value(self) -> i64 {
        self.0
    }

    /// Returns `true` if the amount is strictly positive.
    pub fn is_positive(self) -> bool {
        self.0 > 0
    }

    /// Returns `true` if the amount is strictly negative.
    pub fn is_negative(self) -> bool {
        self.0 < 0
    }

    /// Returns `true` if the amount is zero.
    pub fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Checked addition, returning [`OverflowError`] on overflow.
    pub fn checked_add(self, rhs: Self) -> Result<Self, OverflowError> {
        self.0.checked_add(rhs.0).map(Cent).ok_or(OverflowError)
    }

    /// Checked subtraction, returning [`OverflowError`] on underflow.
    pub fn checked_sub(self, rhs: Self) -> Result<Self, OverflowError> {
        self.0.checked_sub(rhs.0).map(Cent).ok_or(OverflowError)
    }

    /// Checked negation, returning [`OverflowError`] if `self == i64::MIN`.
    pub fn checked_neg(self) -> Result<Self, OverflowError> {
        self.0.checked_neg().map(Cent).ok_or(OverflowError)
    }

    /// Sum an iterator of `Cent` values with overflow checking.
    pub fn checked_sum(iter: impl IntoIterator<Item = Self>) -> Result<Self, OverflowError> {
        let mut sum = Cent::ZERO;
        for x in iter {
            sum = sum.checked_add(x)?;
        }
        Ok(sum)
    }
}

impl From<i64> for Cent {
    fn from(v: i64) -> Self {
        Self(v)
    }
}

impl From<i32> for Cent {
    fn from(v: i32) -> Self {
        Self(v as i64)
    }
}

impl From<u32> for Cent {
    fn from(v: u32) -> Self {
        Self(v as i64)
    }
}

impl From<u8> for Cent {
    fn from(v: u8) -> Self {
        Self(v as i64)
    }
}

impl From<i8> for Cent {
    fn from(v: i8) -> Self {
        Self(v as i64)
    }
}

impl fmt::Debug for Cent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Cent({})", self.0)
    }
}

impl fmt::Display for Cent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl ToBytes for Cent {
    fn to_bytes(&self) -> Vec<u8> {
        self.0.to_be_bytes().to_vec()
    }
}

// ---------------------------------------------------------------------------
// Amount — human-friendly parser/formatter (not stored)
// ---------------------------------------------------------------------------

/// Parses and formats human-readable amounts with a fixed number of decimal
/// places. NOT stored anywhere — used only to convert between strings and
/// [`Cent`] values.
pub struct Amount {
    decimals: u8,
}

/// Error returned when parsing an amount string fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseAmountError {
    /// The input string is not a valid number.
    InvalidFormat(String),
    /// Too many decimal places for the configured precision.
    TooManyDecimals {
        /// Maximum allowed decimal places.
        max: u8,
        /// Number of decimal places found in the input.
        found: usize,
    },
}

impl fmt::Display for ParseAmountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFormat(s) => write!(f, "invalid amount format: {s}"),
            Self::TooManyDecimals { max, found } => {
                write!(f, "too many decimals: max {max}, found {found}")
            }
        }
    }
}

impl std::error::Error for ParseAmountError {}

impl Amount {
    /// Create an `Amount` formatter with the given number of decimal places.
    pub fn new(decimals: u8) -> Self {
        Self { decimals }
    }

    /// Parses a decimal string into a [`Cent`] value.
    pub fn parse(&self, s: &str) -> Result<Cent, ParseAmountError> {
        let s = s.trim();
        let (negative, s) = if let Some(rest) = s.strip_prefix('-') {
            (true, rest)
        } else {
            (false, s)
        };

        let (whole_str, frac_str) = if let Some((w, f)) = s.split_once('.') {
            (w, f)
        } else {
            (s, "")
        };

        if whole_str.is_empty() && frac_str.is_empty() {
            return Err(ParseAmountError::InvalidFormat(s.to_string()));
        }

        let whole: i64 = if whole_str.is_empty() {
            0
        } else {
            whole_str
                .parse()
                .map_err(|_| ParseAmountError::InvalidFormat(s.to_string()))?
        };

        if frac_str.len() > self.decimals as usize {
            return Err(ParseAmountError::TooManyDecimals {
                max: self.decimals,
                found: frac_str.len(),
            });
        }

        if !frac_str.is_empty() && !frac_str.chars().all(|c| c.is_ascii_digit()) {
            return Err(ParseAmountError::InvalidFormat(s.to_string()));
        }

        let frac: i64 = if frac_str.is_empty() {
            0
        } else {
            let padded = format!("{:0<width$}", frac_str, width = self.decimals as usize);
            padded
                .parse()
                .map_err(|_| ParseAmountError::InvalidFormat(s.to_string()))?
        };

        let multiplier = 10i64.pow(self.decimals as u32);
        let value = whole
            .checked_mul(multiplier)
            .and_then(|v| v.checked_add(frac))
            .ok_or_else(|| ParseAmountError::InvalidFormat(s.to_string()))?;

        Ok(Cent::from(if negative {
            value
                .checked_neg()
                .ok_or_else(|| ParseAmountError::InvalidFormat(s.to_string()))?
        } else {
            value
        }))
    }

    /// Formats a [`Cent`] value as a decimal string.
    pub fn format(&self, cent: Cent) -> String {
        if self.decimals == 0 {
            return cent.value().to_string();
        }

        let value = cent.value();
        let negative = value < 0;
        let abs = value.unsigned_abs();
        let multiplier = 10u64.pow(self.decimals as u32);
        let whole = abs / multiplier;
        let frac = abs % multiplier;

        let sign = if negative { "-" } else { "" };
        format!(
            "{sign}{whole}.{frac:0>width$}",
            width = self.decimals as usize
        )
    }
}

// ---------------------------------------------------------------------------
// Debug / Display impls for identifiers
// ---------------------------------------------------------------------------

impl fmt::Debug for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AccountId({})", self.0)
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
        thread_local! {
            static GEN: crate::autoid::AutoId = crate::autoid::AutoId::new();
        }
        GEN.with(|g| Self(g.next()))
    }
}

impl AccountId {
    /// Create an `AccountId` from an `i64`.
    pub const fn new(id: i64) -> Self {
        Self(id)
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
        thread_local! {
            static GEN: crate::autoid::AutoId = crate::autoid::AutoId::new();
        }
        GEN.with(|g| Self(g.next()))
    }
}

/// Identifies a reservation — the owner token stamped on a posting while it is
/// `PendingInactive`, so only the saga that reserved it may finalize or release it.
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
        thread_local! {
            static GEN: crate::autoid::AutoId = crate::autoid::AutoId::new();
        }
        GEN.with(|g| Self(g.next()))
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

/// Lifecycle state of a [`Posting`].
///
/// ```text
/// Active ──reserve──▶ PendingInactive ──finalize──▶ Inactive (void)
///   ▲  ▲                    │
///   │  └─── release (no-op) ┘
///   └────── release ────────┘  (compensation)
/// ```
///
/// `reserve_postings` and `release_postings` are batch operations:
/// - **reserve**: all postings must be Active, otherwise the batch fails.
/// - **release**: Active is a no-op, PendingInactive reverts to Active,
///   Inactive (void) fails the batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PostingStatus {
    /// Available for consumption and counted in balance.
    Active,
    /// Reserved for a transfer; not available for other consumption.
    /// Reverts to `Active` on compensation via `release_postings`.
    PendingInactive,
    /// Consumed by a committed transfer. Kept for audit trail (void).
    /// Cannot be released.
    Inactive,
}

/// A signed amount of one asset, owned by exactly one account.
///
/// A positive posting is value controlled by the account; a negative posting is
/// an offset position (issuance, external flow, overdraft, or system balancing).
/// Negative postings are allowed on every policy except `NoOverdraft`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Posting {
    /// Unique identifier derived from the creating transfer.
    pub id: PostingId,
    /// The account that owns this posting.
    pub owner: AccountId,
    /// The asset this posting denominates.
    pub asset: AssetId,
    /// Signed: positive = value controlled by the account, negative = offset position.
    pub value: Cent,
    /// Lifecycle state — only `Active` postings count toward balance.
    pub status: PostingStatus,
    /// Owner token while `PendingInactive`. `Some(rid)` iff reserved by saga
    /// `rid`; `None` when `Active` or `Inactive`. Only the holder of a matching
    /// `ReservationId` may finalize or release a reserved posting.
    pub reservation: Option<ReservationId>,
}

impl Posting {
    /// Construct an `Active`, unreserved posting.
    pub fn new(id: PostingId, owner: AccountId, asset: AssetId, value: Cent) -> Self {
        Self {
            id,
            owner,
            asset,
            value,
            status: PostingStatus::Active,
            reservation: None,
        }
    }

    /// Returns `true` if this posting's status is [`PostingStatus::Active`].
    pub fn is_active(&self) -> bool {
        self.status == PostingStatus::Active
    }
}

/// A posting to be created — carries no id yet because the [`PostingId`] depends
/// on the [`EnvelopeId`], which is computed during validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewPosting {
    /// The account that will own the created posting.
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

/// Fixed-width secondary identifiers.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UserData {
    /// 128-bit user-defined slot (e.g. external UUID).
    pub d128: u128,
    /// 64-bit user-defined slot (e.g. correlation id).
    pub d64: u64,
    /// 32-bit user-defined slot (e.g. category code).
    pub d32: u32,
}

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
    /// Fixed-width secondary identifiers.
    pub user_data: UserData,
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

    /// Fixed-width secondary identifiers.
    pub fn user_data(&self) -> &UserData {
        &self.user_data
    }

    /// Free-form key-value metadata.
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// Deduplicated, sorted list of accounts referenced in the created postings.
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

    /// Set the fixed-width secondary identifiers.
    pub fn user_data(mut self, user_data: UserData) -> Self {
        self.envelope.user_data = user_data;
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
        // Bits 2–7: reserved for future system flags.
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
    /// Stable identity for this account.
    pub id: AccountId,
    /// Monotonically increasing version, starts at 1 on creation.
    pub version: u64,
    /// Overdraft / balance policy.
    pub policy: AccountPolicy,
    /// Lifecycle flags (frozen, closed).
    pub flags: AccountFlags,
    /// Book this entity belongs to.
    pub book: BookId,
    /// Fixed-width secondary identifiers.
    pub user_data: UserData,
    /// Free-form key-value metadata.
    pub metadata: Metadata,
}

impl Account {
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
    /// Account being debited.
    pub from: AccountId,
    /// Account being credited.
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
    /// Fixed-width secondary identifiers.
    pub user_data: UserData,
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

    /// Add a raw movement.
    pub fn movement(
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

    /// Add a pay movement: transfer value between two accounts.
    pub fn pay(self, from: AccountId, to: AccountId, asset: AssetId, amount: Cent) -> Self {
        self.movement(from, to, asset, amount)
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

    /// Set the fixed-width secondary identifiers.
    pub fn user_data(mut self, user_data: UserData) -> Self {
        self.transfer.user_data = user_data;
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
        self.0.to_be_bytes().to_vec()
    }
}

impl ToBytes for AccountSnapshotId {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(40);
        buf.extend_from_slice(&self.account.0.to_be_bytes());
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

impl ToBytes for UserData {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(28);
        write_u128(&mut buf, self.d128);
        write_u64(&mut buf, self.d64);
        write_u32(&mut buf, self.d32);
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
        buf.push(match self.status {
            PostingStatus::Active => 0,
            PostingStatus::PendingInactive => 1,
            PostingStatus::Inactive => 2,
        });
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
        buf.extend(self.user_data.to_bytes());

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
        buf.extend(self.user_data.to_bytes());

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
