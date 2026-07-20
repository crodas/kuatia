//! Storage abstraction separating the pure decision logic from IO.
//!
//! The [`Store`] trait composes focused sub-traits, each a dumb instruction
//! follower: writes apply one update and report an affected-row count (or an I/O
//! error). The saga, not the store, interprets counts and owns idempotency and
//! compensation.
//! - [`AccountStore`] — account CRUD and versioning
//! - [`PostingStore`] — posting reads and lifecycle transitions
//! - [`TransferStore`] — transfer persistence and queries
//! - [`SagaStore`] — saga state for crash recovery
//! - [`EventStore`] — the ledger event log
//! - [`BookStore`] — book persistence
//! - [`BalanceProjectionStore`] — the cached balance projection (ADR-0019)

use async_trait::async_trait;
use kuatia_types::{
    Account, AccountId, AssetId, Book, BookId, Cent, Envelope, EnvelopeId, Posting, PostingFilter,
    PostingId, PostingState, Receipt, ReservationId,
};

use crate::error::StoreError;
use crate::events::EventStore;

/// Pairs a committed transfer with its receipt.
#[derive(Debug, Clone)]
pub struct EnvelopeRecord {
    /// The envelope that was committed.
    pub envelope: Envelope,
    /// The receipt proving commitment.
    pub receipt: Receipt,
    /// Unix milliseconds when this record was created.
    pub created_at: i64,
}

/// Pagination and filtering parameters for posting queries.
#[derive(Debug, Clone)]
pub struct PostingQuery {
    /// Filter to postings owned by this base account.
    pub account: i64,
    /// Restrict to one subaccount; `None` spans every subaccount of `account`.
    pub sub: Option<i64>,
    /// Filter by asset.
    pub asset: Option<AssetId>,
    /// Filter by derived lifecycle state.
    pub filter: PostingFilter,
    /// Max results to return.
    pub limit: Option<u32>,
    /// Number of results to skip.
    pub offset: Option<u32>,
}

/// Pagination and filtering parameters for transfer queries.
#[derive(Debug, Clone, Default)]
pub struct TransferQuery {
    /// Filter to transfers involving this base account.
    pub account: Option<i64>,
    /// Restrict to one subaccount; `None` spans every subaccount of `account`.
    pub sub: Option<i64>,
    /// Inclusive lower bound (unix millis).
    pub from_ts: Option<i64>,
    /// Exclusive upper bound (unix millis).
    pub to_ts: Option<i64>,
    /// Filter by book.
    pub book: Option<BookId>,
    /// Max results to return.
    pub limit: Option<u32>,
    /// Number of results to skip.
    pub offset: Option<u32>,
}

/// A page of results with total count for pagination.
#[derive(Debug, Clone)]
pub struct Page<T> {
    /// The items in this page.
    pub items: Vec<T>,
    /// Total number of matching items (before pagination).
    pub total: u64,
}

// ---------------------------------------------------------------------------
// Sub-traits
// ---------------------------------------------------------------------------

/// Account persistence: create, version, query.
#[async_trait]
pub trait AccountStore: Send + Sync {
    /// Fetch a single account by id.
    async fn get_account(&self, id: &AccountId) -> Result<Account, StoreError>;
    /// Fetch multiple accounts by id.
    async fn get_accounts(&self, ids: &[AccountId]) -> Result<Vec<Account>, StoreError>;
    /// Persist a new account (version 1).
    async fn create_account(&self, account: Account) -> Result<(), StoreError>;
    /// Append a new version to an existing account.
    async fn append_account_version(&self, account: Account) -> Result<(), StoreError>;
    /// Return the full version history for an account.
    async fn get_account_history(&self, id: &AccountId) -> Result<Vec<Account>, StoreError>;
    /// List all accounts (latest version of each).
    async fn list_accounts(&self) -> Result<Vec<Account>, StoreError>;
}

/// Posting persistence: an immutable posting table plus two id-only index
/// tables (active, reserved) whose membership expresses lifecycle state.
///
/// A posting is written once into the immutable table and never updated. Its
/// state is derived from which index it is in: active → spendable, reserved →
/// claimed by a saga, neither → spent. All lifecycle transitions are inserts
/// and deletes on the index tables; the posting row itself never changes.
#[async_trait]
pub trait PostingStore: Send + Sync {
    /// Fetch postings by their ids from the immutable table. Consumed (spent)
    /// postings still resolve here — immutable rows persist forever.
    async fn get_postings(&self, ids: &[PostingId]) -> Result<Vec<Posting>, StoreError>;
    /// Return postings owned by a base account, filtered by subaccount, asset,
    /// and derived lifecycle state (see [`PostingFilter`]). `sub == None` spans
    /// every subaccount of `id`; `sub == Some(s)` restricts to that one. Results
    /// are ordered by posting id, so callers and pagination built on top see a
    /// stable sequence.
    async fn get_postings_by_account(
        &self,
        id: i64,
        sub: Option<i64>,
        asset: Option<&AssetId>,
        filter: PostingFilter,
    ) -> Result<Vec<Posting>, StoreError>;
    /// Return the derived [`PostingState`] of each id, aligned to the input
    /// order. This is the single membership probe used by the saga verifier and
    /// the finalize guards. An id absent from the immutable table is `Missing`.
    async fn get_posting_states(&self, ids: &[PostingId]) -> Result<Vec<PostingState>, StoreError>;
    /// Reserve postings: move each id from the active index into the reserved
    /// index under `reservation`. A dumb instruction — an id moves only if it
    /// was in the active index (the delete-returns-one is the atomic
    /// single-winner claim under contention); returns the **number of rows
    /// reserved** (0 ≤ n ≤ ids.len()). The caller (saga) interprets a short
    /// count.
    async fn reserve_postings(
        &self,
        ids: &[PostingId],
        reservation: ReservationId,
    ) -> Result<u64, StoreError>;
    /// Release postings: move each id reserved by `reservation` back into the
    /// active index. A dumb instruction — only ids reserved by this
    /// `reservation` move; returns the **number of rows released**. Releasing an
    /// id that is already active or reserved by another saga does not count.
    async fn release_postings(
        &self,
        ids: &[PostingId],
        reservation: ReservationId,
    ) -> Result<u64, StoreError>;

    /// Deactivate postings: remove each id from an index so it becomes spent
    /// (present only in the immutable table). A dumb instruction — it applies
    /// the conditional delete and returns the **number of rows removed**.
    /// - `reservation == None` (raw): remove ids from the active index.
    /// - `reservation == Some(rid)`: remove ids from the reserved index that are
    ///   owned by `rid`.
    /// Returns the count actually removed (0 ≤ n ≤ ids.len()).
    async fn deactivate_postings(
        &self,
        ids: &[PostingId],
        reservation: Option<ReservationId>,
    ) -> Result<u64, StoreError>;

    /// Insert postings if absent, and add each newly-inserted id to the active
    /// index. A dumb instruction — inserts each posting into the immutable table
    /// unless one with the same id already exists, activating only the rows that
    /// were newly inserted, and returns the **number of immutable rows
    /// inserted** (already-present postings contribute 0, and do not get
    /// re-activated). The caller decides what a short count means.
    async fn insert_postings(&self, postings: &[Posting]) -> Result<u64, StoreError>;

    /// Query postings with filtering and pagination.
    async fn query_postings(&self, query: &PostingQuery) -> Result<Page<Posting>, StoreError> {
        // `get_postings_by_account` returns a deterministic id-ordered sequence,
        // so paginating here is stable without an extra sort. A backend that can
        // push the window into its query (e.g. SQL `LIMIT`) overrides this.
        let all = self
            .get_postings_by_account(query.account, query.sub, query.asset.as_ref(), query.filter)
            .await?;
        Ok(crate::query::paginate(all, query.offset, query.limit))
    }
}

/// Transfer persistence: store and query committed transfers.
#[async_trait]
pub trait TransferStore: Send + Sync {
    /// Fetch a transfer record by its content-addressed id.
    async fn get_transfer(&self, id: &EnvelopeId) -> Result<Option<EnvelopeRecord>, StoreError>;
    /// Persist a transfer record if absent (idempotent) and index it under every
    /// account in `involved` (both created and consumed owners — the caller
    /// supplies the set so storage computes nothing). Every backend indexes the
    /// transfer under exactly these accounts and derives participation from
    /// nowhere else, so `get_transfers_for_account` returns the same set of
    /// transfers regardless of backend. A dumb instruction: returns **1** if the
    /// transfer row was newly inserted, **0** if it already existed. The caller
    /// decides what `0` means.
    async fn store_transfer(
        &self,
        record: EnvelopeRecord,
        involved: &[AccountId],
    ) -> Result<u64, StoreError>;
    /// Return all transfers involving the given base account. `sub == None`
    /// spans every subaccount of `id`; `sub == Some(s)` restricts to one.
    async fn get_transfers_for_account(
        &self,
        id: i64,
        sub: Option<i64>,
    ) -> Result<Vec<EnvelopeRecord>, StoreError>;

    /// Query transfers with filtering and pagination.
    ///
    /// A backend loads the candidate records, then hands them to
    /// [`filter_transfers`](crate::query::filter_transfers) and
    /// [`paginate`](crate::query::paginate) for the shared time-window/book and
    /// page cut. There is no default because loading candidates differs by
    /// backend and, crucially, an `account == None` query must be answered the
    /// same way everywhere: a store-wide scan, not an error. Every backend does
    /// exactly that.
    async fn query_transfers(
        &self,
        query: &TransferQuery,
    ) -> Result<Page<EnvelopeRecord>, StoreError>;
}

/// Saga state persistence for crash recovery.
#[async_trait]
pub trait SagaStore: Send + Sync {
    /// Persist a saga execution state.
    async fn save_saga(&self, id: &i64, data: Vec<u8>) -> Result<(), StoreError>;
    /// Load all pending (incomplete) saga states.
    async fn list_pending_sagas(&self) -> Result<Vec<(i64, Vec<u8>)>, StoreError>;
    /// Delete a completed saga state.
    async fn delete_saga(&self, id: &i64) -> Result<(), StoreError>;
}

/// Book persistence.
#[async_trait]
pub trait BookStore: Send + Sync {
    /// Create a new book.
    async fn create_book(&self, book: Book) -> Result<(), StoreError>;
    /// Fetch a book by id.
    async fn get_book(&self, id: &BookId) -> Result<Book, StoreError>;
    /// List all books.
    async fn list_books(&self) -> Result<Vec<Book>, StoreError>;
}

/// One append-only cache point (ADR-0019): a balance snapshot for one
/// `(account, asset)` and the commit-time watermark it covers, tagged with a
/// monotonic `id`. Cache points are never updated; a read selects the highest
/// `id`. A disposable, rebuildable accelerator, never the source of truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceProjection {
    /// Monotonic cache-point id (a store-minted snowflake). Higher means newer.
    pub id: i64,
    /// Account (base id plus subaccount) this cache point is for.
    pub account: AccountId,
    /// Asset this cache point is for.
    pub asset: AssetId,
    /// Cached balance: the sum of every committed transfer's delta for this
    /// `(account, asset)` with commit time at or before `watermark`.
    pub balance: Cent,
    /// Commit-time cutoff (unix millis) the snapshot covers. A read folds in
    /// committed transfers with a commit time strictly greater than this.
    pub watermark: i64,
}

/// Append-only balance cache points (ADR-0019). A disposable, rebuildable read
/// accelerator; the append-only postings stay the source of truth, and this is
/// never consulted for the validate-time overdraft check. Cache points are only
/// ever appended (never updated), and a read takes the one closest to (at or
/// before) the target time, so concurrent appends need no lock.
#[async_trait]
pub trait BalanceProjectionStore: Send + Sync {
    /// Append a new cache point for `(account, asset)`, minting a fresh monotonic
    /// `id`. A dumb instruction: it inserts one row and never updates an existing
    /// one.
    async fn append_balance_projection(
        &self,
        account: &AccountId,
        asset: &AssetId,
        balance: Cent,
        watermark: i64,
    ) -> Result<(), StoreError>;

    /// Fetch the cache point for `(account, asset)` closest to `as_of`: the one
    /// with the largest `watermark` at or before `as_of` (tie-broken by highest
    /// `id`). This is the freshest snapshot a read as of `as_of` may use without
    /// covering transfers committed after `as_of`. Returns `None` if no cache
    /// point is at or before `as_of`. The caller supplies `as_of` (the store has
    /// no clock); the ledger passes the current time by default.
    async fn get_closest_balance_projection(
        &self,
        account: &AccountId,
        asset: &AssetId,
        as_of: i64,
    ) -> Result<Option<BalanceProjection>, StoreError>;
}

// ---------------------------------------------------------------------------
// Composite trait
// ---------------------------------------------------------------------------

/// Async storage abstraction composing all sub-traits.
pub trait Store:
    AccountStore
    + PostingStore
    + TransferStore
    + SagaStore
    + EventStore
    + BookStore
    + BalanceProjectionStore
{
}

impl<
    T: AccountStore
        + PostingStore
        + TransferStore
        + SagaStore
        + EventStore
        + BookStore
        + BalanceProjectionStore,
> Store for T
{
}
