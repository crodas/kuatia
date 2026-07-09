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
use std::sync::atomic::{AtomicU64, Ordering};

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

impl fmt::Display for AccountId {
    /// IBAN-style machine format: a fixed 20 characters, an 18-character base-36
    /// body followed by two trailing ISO 7064 mod-97 check digits, with no
    /// country code. The `(id, sub)` pair is packed into a 93-bit value and run
    /// through a keyed format-preserving permutation (see [`set_id_seed`]) before
    /// encoding, so the body does not reveal the raw ids. Round-trips via
    /// [`FromStr`](std::str::FromStr); [`to_grouped`](AccountId::to_grouped) adds
    /// the presentation spacing (five groups of four).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let body = base36_body(obfuscate(pack(self.id, self.sub), id_seed()));
        write!(f, "{body}{:02}", check_digits(&body))
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
// IBAN-style string view for AccountId (ADR-0012)
// ---------------------------------------------------------------------------

/// Bits of the base `id` the string form encodes. A snowflake id keeps the sign
/// bit clear, so its value fits in 63 bits.
pub const ID_BITS: u32 = 63;

/// Bits of the `sub` leg the string form encodes. Together with [`ID_BITS`] this
/// is the 93-bit packed value the code carries, which fits in the 18 base-36
/// characters of the code body. A subaccount id must fit in this many bits to
/// round-trip through the string form.
pub const SUB_BITS: u32 = 30;

/// Number of base-36 characters in the code body. `36^18 > 2^93`, so the packed
/// `(id, sub)` value always fits.
const BODY_LEN: usize = 18;

/// Mask selecting the low [`ID_BITS`] + [`SUB_BITS`] = 93 bits of the packed value.
const PACK_MASK: u128 = (1u128 << (ID_BITS + SUB_BITS)) - 1;

/// Pack `(id, sub)` into the 93-bit value the code encodes: the low [`ID_BITS`]
/// of `id` in the high part, the low [`SUB_BITS`] of `sub` in the low part.
fn pack(id: i64, sub: i64) -> u128 {
    let id = (id as u128) & ((1u128 << ID_BITS) - 1);
    let sub = (sub as u128) & ((1u128 << SUB_BITS) - 1);
    (id << SUB_BITS) | sub
}

/// Inverse of [`pack`]: split the 93-bit value back into the two legs. The `id`
/// part is at most 63 bits, so it is always a non-negative `i64`.
fn unpack(p: u128) -> (i64, i64) {
    let sub = (p & ((1u128 << SUB_BITS) - 1)) as i64;
    let id = (p >> SUB_BITS) as i64;
    (id, sub)
}

/// Encode a value `< 2^93` as exactly [`BODY_LEN`] base-36 digits (`0-9A-Z`),
/// zero-padded on the left. `36^18 > 2^93`, so this never truncates.
fn base36_body(mut v: u128) -> String {
    const D: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut out = [b'0'; BODY_LEN];
    let mut i = out.len();
    while v > 0 && i > 0 {
        i -= 1;
        out[i] = D[(v % 36) as usize];
        v /= 36;
    }
    out.iter().map(|&b| b as char).collect()
}

/// Expand an IBAN string to its numeric form for the checksum: digits stay,
/// letters `A-Z` become `10..35`.
fn iban_expand(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.bytes() {
        if c.is_ascii_digit() {
            out.push(c as char);
        } else {
            let v = (c - b'A') as u32 + 10;
            out.push_str(&v.to_string());
        }
    }
    out
}

/// ISO 7064 mod-97-10 over a decimal string, computed iteratively so the input
/// length is unbounded.
fn mod97(digits: &str) -> u32 {
    let mut rem = 0u32;
    for b in digits.bytes() {
        rem = (rem * 10 + (b - b'0') as u32) % 97;
    }
    rem
}

/// The two mod-97 check digits for a base-36 body, IBAN-style but with no
/// country code: `98 - (expand(body ++ "00") mod 97)`.
fn check_digits(body: &str) -> u32 {
    98 - mod97(&iban_expand(&format!("{body}00")))
}

// ---------------------------------------------------------------------------
// Account-code obfuscation (ADR-0012, ADR-0015)
//
// The account code's body is a base-36 rendering of the packed (id, sub) value.
// Without mixing, small ids render as long runs of zeros that reveal their value
// and sequence. To hide that from outsiders, the 93-bit packed value is run
// through a keyed format-preserving permutation before encoding, and inverted on
// parse. The permutation is a Feistel network over 2^94 restricted to the 93-bit
// domain by cycle-walking (re-encrypt while the result exceeds the domain), which
// keeps it a bijection on the exact set of packable values.
//
// This is obfuscation, not security: anyone with the seed can decode it, so it
// is not a substitute for authorization. The seed has a default and can be set
// once at startup via `set_id_seed`; changing it changes every code, so it must
// be stable across a deployment.
// ---------------------------------------------------------------------------

/// Default seed for the account-code obfuscation permutation. Override at
/// startup with [`set_id_seed`], before any code is issued or parsed.
pub const DEFAULT_ID_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// Process-global seed keying the account-code permutation.
static ID_SEED: AtomicU64 = AtomicU64::new(DEFAULT_ID_SEED);

/// Set the process-global seed that keys the account-code obfuscation. Call once
/// at startup: every [`AccountId`] string form depends on it, so changing it
/// after codes are issued invalidates the previously issued ones.
pub fn set_id_seed(seed: u64) {
    ID_SEED.store(seed, Ordering::Relaxed);
}

/// The current process-global account-code seed.
pub fn id_seed() -> u64 {
    ID_SEED.load(Ordering::Relaxed)
}

/// Number of Feistel rounds. Four rounds of a strong round function give a
/// strong pseudo-random permutation (Luby-Rackoff), which is ample for
/// obfuscation.
const FEISTEL_ROUNDS: usize = 4;

/// SplitMix64 finalizer: a strong 64-bit avalanche mixer.
fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Per-round subkey derived from the seed and round index.
fn round_key(seed: u64, round: usize) -> u64 {
    mix64(seed ^ (round as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

/// Bits per Feistel half. Two halves give a 94-bit block, the smallest even
/// width that covers the 93-bit packed domain (one bit of headroom that
/// cycle-walking absorbs).
const FEISTEL_HALF_BITS: u32 = 47;

/// Mask selecting one [`FEISTEL_HALF_BITS`]-wide half.
const FEISTEL_HALF_MASK: u128 = (1u128 << FEISTEL_HALF_BITS) - 1;

/// Keyed Feistel permutation over the 94-bit block `(l, r)` (two 47-bit halves).
/// A balanced Feistel with any round function is a bijection on the full 2^94
/// space, which is what [`obfuscate`] cycle-walks down to the 93-bit domain.
fn feistel94(block: u128, seed: u64) -> u128 {
    let mut l = (block >> FEISTEL_HALF_BITS) & FEISTEL_HALF_MASK;
    let mut r = block & FEISTEL_HALF_MASK;
    for round in 0..FEISTEL_ROUNDS {
        let f = (mix64(r as u64 ^ round_key(seed, round)) as u128) & FEISTEL_HALF_MASK;
        let next = (l ^ f) & FEISTEL_HALF_MASK;
        l = r;
        r = next;
    }
    (l << FEISTEL_HALF_BITS) | r
}

/// Inverse of [`feistel94`] under the same seed.
fn feistel94_inv(block: u128, seed: u64) -> u128 {
    let mut l = (block >> FEISTEL_HALF_BITS) & FEISTEL_HALF_MASK;
    let mut r = block & FEISTEL_HALF_MASK;
    for round in (0..FEISTEL_ROUNDS).rev() {
        let f = (mix64(l as u64 ^ round_key(seed, round)) as u128) & FEISTEL_HALF_MASK;
        let prev = (r ^ f) & FEISTEL_HALF_MASK;
        r = l;
        l = prev;
    }
    (l << FEISTEL_HALF_BITS) | r
}

/// Keyed format-preserving permutation over the 93-bit packed domain. Applies
/// [`feistel94`] and cycle-walks (re-encrypts while the result exceeds the
/// domain) so the output is always a valid packed value. Since the domain is
/// half of 2^94, this averages about two iterations and always terminates.
fn obfuscate(p: u128, seed: u64) -> u128 {
    let mut v = p;
    loop {
        v = feistel94(v, seed);
        if v <= PACK_MASK {
            return v;
        }
    }
}

/// Inverse of [`obfuscate`] under the same seed: cycle-walk [`feistel94_inv`].
fn deobfuscate(y: u128, seed: u64) -> u128 {
    let mut v = y;
    loop {
        v = feistel94_inv(v, seed);
        if v <= PACK_MASK {
            return v;
        }
    }
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

    /// IBAN-style presentation format: the machine [`Display`](fmt::Display)
    /// form grouped into blocks of four with a single space. The code is a fixed
    /// 20 characters, so this is always five groups of four
    /// (e.g. `K3P9 WM2X Q7ND V8HT 2R47`).
    pub fn to_grouped(&self) -> String {
        let machine = self.to_string();
        let mut out = String::with_capacity(machine.len() + machine.len() / 4);
        for (i, c) in machine.chars().enumerate() {
            if i > 0 && i % 4 == 0 {
                out.push(' ');
            }
            out.push(c);
        }
        out
    }
}

impl From<AccountSnapshotId> for AccountId {
    fn from(snap: AccountSnapshotId) -> Self {
        snap.account
    }
}

/// Returned when a string is not a valid [`AccountId`] code: wrong structure,
/// non-base-36 body, or a failed mod-97 checksum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseAccountIdError;

impl fmt::Display for ParseAccountIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid AccountId: not a checksum-valid account code")
    }
}

impl std::error::Error for ParseAccountIdError {}

impl std::str::FromStr for AccountId {
    type Err = ParseAccountIdError;

    /// Parse an IBAN-style account code back into the two legs. Any spaces
    /// (grouped display format) and dashes (URL-safe separator) are ignored and
    /// the input is upper-cased first, so `K3P9...`, `K3P9 WM2X ...`, and
    /// `K3P9-WM2X-...` all parse to the same id. The value must reduce to an
    /// 18-character base-36 body followed by two check digits, the ISO 7064
    /// mod-97 checksum must pass, and the decoded body must lie in the packable
    /// domain — so a mistyped or otherwise invalid id is rejected here rather
    /// than reaching the store.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let cleaned: String = s
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .map(|c| c.to_ascii_uppercase())
            .collect();
        // 18-char base-36 body + 2 trailing check digits.
        if cleaned.len() != BODY_LEN + 2 {
            return Err(ParseAccountIdError);
        }
        let body = &cleaned[0..BODY_LEN];
        let check = &cleaned[BODY_LEN..BODY_LEN + 2];
        let is_base36 = |b: u8| b.is_ascii_digit() || b.is_ascii_uppercase();
        if !check.bytes().all(|b| b.is_ascii_digit()) || !body.bytes().all(is_base36) {
            return Err(ParseAccountIdError);
        }
        // Checksum-valid iff the expanded (body ++ check) reduces to 1 under
        // mod-97.
        if mod97(&iban_expand(&format!("{body}{check}"))) != 1 {
            return Err(ParseAccountIdError);
        }
        // Decode the body, reject anything outside the 93-bit packable domain
        // (the permutation's image), then invert the permutation and unpack.
        let obf = u128::from_str_radix(body, 36).map_err(|_| ParseAccountIdError)?;
        if obf > PACK_MASK {
            return Err(ParseAccountIdError);
        }
        let (id, sub) = unpack(deobfuscate(obf, id_seed()));
        Ok(Self { id, sub })
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
    /// The account (subaccount) that owns this posting.
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

#[cfg(test)]
mod account_id_tests {
    use super::*;
    use std::str::FromStr;

    /// Largest subaccount id that round-trips through the string form.
    const MAX_SUB: i64 = (1i64 << SUB_BITS) - 1;

    #[test]
    fn code_structure() {
        let s = AccountId::with_sub(5, 7).to_string();
        // Fixed 20 chars: an 18-char base-36 body then two check digits, no
        // country code.
        assert_eq!(s.len(), 20);
        assert_eq!(s.len(), BODY_LEN + 2);
        // The two trailing check digits are numeric.
        assert!(s[BODY_LEN..].bytes().all(|b| b.is_ascii_digit()));
        // The body is permuted, so it does NOT expose the raw legs the way an
        // unmixed base-36 rendering (a run of zeros then "5"/"7") would.
        assert_ne!(&s[..BODY_LEN], "000000000000000527");
    }

    #[test]
    fn code_round_trips() {
        for acc in [
            AccountId::new(0),
            AccountId::new(100),
            AccountId::with_sub(5, 7),
            AccountId::with_sub(987654321, 12345),
            // The corners of the encodable domain: a full 63-bit id and the
            // widest subaccount.
            AccountId::new(i64::MAX),
            AccountId::with_sub(i64::MAX, MAX_SUB),
            AccountId::with_sub(0, MAX_SUB),
        ] {
            let s = acc.to_string();
            assert_eq!(s.len(), 20, "length {s}");
            assert_eq!(AccountId::from_str(&s).unwrap(), acc, "round-trip {s}");
        }
    }

    #[test]
    fn out_of_range_legs_truncate_to_the_encodable_domain() {
        // The string form encodes only the low ID_BITS / SUB_BITS. A subaccount
        // one past the encodable range wraps to 0 (documented narrowing), so it
        // shares its code with the main account rather than corrupting it.
        let over = AccountId::with_sub(7, 1i64 << SUB_BITS);
        assert_eq!(
            AccountId::from_str(&over.to_string()).unwrap(),
            AccountId::new(7)
        );
    }

    #[test]
    fn parses_a_fixed_vector() {
        // A hardcoded, checksum-valid code (under DEFAULT_ID_SEED) pins the
        // exact encoding, permutation, and checksum, so an accidental change to
        // any of them is caught by a failing parse.
        let code = "KUJLQEL8IX2XGTBK4425";
        let expected = AccountId::with_sub(987654321, 12345);
        assert_eq!(AccountId::from_str(code).unwrap(), expected);
        // The grouped (spaced, lower-cased) form parses to the same value.
        assert_eq!(
            AccountId::from_str("kujl qel8 ix2x gtbk 4425").unwrap(),
            expected
        );
        // Display reproduces the exact machine form.
        assert_eq!(expected.to_string(), code);
    }

    #[test]
    fn obfuscation_permutation_is_invertible_across_seeds() {
        for &seed in &[0u64, 1, DEFAULT_ID_SEED, u64::MAX] {
            for &p in &[0u128, 1, 5 << SUB_BITS | 7, PACK_MASK, PACK_MASK - 1] {
                let y = obfuscate(p, seed);
                assert!(y <= PACK_MASK, "output escapes the domain: {y}");
                assert_eq!(deobfuscate(y, seed), p, "seed={seed} p={p}");
            }
        }
    }

    #[test]
    fn obfuscation_hides_structure() {
        // The default seed is in force.
        assert_eq!(id_seed(), DEFAULT_ID_SEED);
        // Sequential base ids do not produce visibly related codes (avalanche).
        let a = AccountId::new(100).to_string();
        let b = AccountId::new(101).to_string();
        let shared = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
        assert!(shared < 4, "codes share too long a prefix: {a} vs {b}");
        // A base account and its subaccount are likewise not obviously related.
        let main = AccountId::new(100).to_string();
        let sub = AccountId::with_sub(100, 1).to_string();
        assert_ne!(main, sub);
    }

    #[test]
    fn grouped_format_is_five_groups_of_four_and_re_parses() {
        let acc = AccountId::with_sub(5, 7);
        let grouped = acc.to_grouped();
        let groups: Vec<&str> = grouped.split(' ').collect();
        assert_eq!(groups.len(), 5);
        assert!(groups.iter().all(|g| g.len() == 4));
        // Grouped format (with spaces) and lower case both parse back.
        assert_eq!(AccountId::from_str(&grouped).unwrap(), acc);
        assert_eq!(AccountId::from_str(&grouped.to_lowercase()).unwrap(), acc);
    }

    #[test]
    fn parses_with_spaces_or_dashes_for_url_safety() {
        let acc = AccountId::with_sub(987654321, 12345);
        let machine = acc.to_string(); // 20 chars, no separators (URL-safe)
        // The same code grouped with spaces (display) or dashes (URL-safe
        // separator) parses back to the same id, as does a mixed/irregular form.
        let spaced = acc.to_grouped();
        let dashed = spaced.replace(' ', "-");
        let mixed = format!("{}-{} {}", &machine[0..4], &machine[4..12], &machine[12..]);
        for s in [&machine, &spaced, &dashed, &mixed] {
            assert_eq!(AccountId::from_str(s).unwrap(), acc, "parse {s}");
        }
    }

    #[test]
    fn from_str_rejects_bad_checksum_and_junk() {
        let good = AccountId::with_sub(5, 7).to_string();
        assert!(AccountId::from_str(&good).is_ok());

        // A helper to overwrite one character while keeping the length.
        let with_char_at = |i: usize, c: char| {
            let mut v: Vec<char> = good.chars().collect();
            v[i] = c;
            v.into_iter().collect::<String>()
        };

        // Flip the last check digit: still numeric and right length, but the
        // checksum no longer matches, so it is rejected.
        let last = good.len() - 1;
        let flipped = with_char_at(last, if good.ends_with('8') { '9' } else { '8' });
        assert!(AccountId::from_str(&flipped).is_err(), "bad checksum");

        // Structurally malformed inputs are all rejected.
        assert!(AccountId::from_str("").is_err(), "empty");
        assert!(AccountId::from_str("not-a-code").is_err(), "junk");
        assert!(AccountId::from_str(&good[..19]).is_err(), "too short");
        assert!(
            AccountId::from_str(&format!("{good}0")).is_err(),
            "too long"
        );
        // A check digit that is not a digit (the last two positions).
        assert!(
            AccountId::from_str(&with_char_at(BODY_LEN, 'A')).is_err(),
            "alpha check"
        );
        // A non-base-36 character in the body (survives space/dash stripping).
        assert!(
            AccountId::from_str(&with_char_at(5, '*')).is_err(),
            "non-base36 body"
        );
    }
}
