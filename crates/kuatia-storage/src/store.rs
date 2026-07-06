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

use async_trait::async_trait;
use kuatia_types::{
    Account, AccountId, AssetId, Book, BookId, Envelope, EnvelopeId, Posting, PostingId,
    PostingStatus, Receipt, ReservationId,
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
    /// Filter by posting status.
    pub status: Option<PostingStatus>,
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

/// Posting persistence: reads and lifecycle transitions.
#[async_trait]
pub trait PostingStore: Send + Sync {
    /// Fetch postings by their ids.
    async fn get_postings(&self, ids: &[PostingId]) -> Result<Vec<Posting>, StoreError>;
    /// Return postings owned by a base account, optionally filtered by
    /// subaccount, asset, and/or status. `sub == None` spans every subaccount
    /// of `id`; `sub == Some(s)` restricts to that one subaccount.
    async fn get_postings_by_account(
        &self,
        id: i64,
        sub: Option<i64>,
        asset: Option<&AssetId>,
        status: Option<PostingStatus>,
    ) -> Result<Vec<Posting>, StoreError>;
    /// Reserve postings: `Active → PendingInactive`, stamping `reservation` as
    /// the owner token. A dumb instruction — each id flips only if still `Active`;
    /// returns the **number of rows reserved** (0 ≤ n ≤ ids.len()). It does not
    /// error on a short count; the caller (saga) interprets it.
    async fn reserve_postings(
        &self,
        ids: &[PostingId],
        reservation: ReservationId,
    ) -> Result<u64, StoreError>;
    /// Release postings: `PendingInactive` owned by `reservation` → `Active`,
    /// clearing the owner. A dumb instruction — only postings reserved by this
    /// `reservation` flip; returns the **number of rows released**. Releasing an
    /// `Active` (already released) or differently-owned posting simply does not
    /// count. The caller interprets the result.
    async fn release_postings(
        &self,
        ids: &[PostingId],
        reservation: ReservationId,
    ) -> Result<u64, StoreError>;

    /// Deactivate postings: flip to `Inactive`. A dumb instruction — it applies
    /// the conditional update and returns the **number of rows changed**; it does
    /// not decide whether that count is correct. The caller (saga) interprets it.
    /// - `reservation == None` (raw): only postings still `Active` flip.
    /// - `reservation == Some(rid)`: only postings `PendingInactive` owned by
    ///   `rid` flip.
    /// Returns the count of postings actually transitioned (0 ≤ n ≤ ids.len()).
    async fn deactivate_postings(
        &self,
        ids: &[PostingId],
        reservation: Option<ReservationId>,
    ) -> Result<u64, StoreError>;

    /// Insert postings if absent (idempotent). A dumb instruction — inserts each
    /// posting unless one with the same id already exists, and returns the
    /// **number of rows inserted** (already-present postings contribute 0). The
    /// caller decides what a short count means.
    async fn insert_postings(&self, postings: &[Posting]) -> Result<u64, StoreError>;

    /// Query postings with filtering and pagination.
    async fn query_postings(&self, query: &PostingQuery) -> Result<Page<Posting>, StoreError> {
        let all = self
            .get_postings_by_account(query.account, query.sub, query.asset.as_ref(), query.status)
            .await?;
        let total = all.len() as u64;
        let offset = query.offset.unwrap_or(0) as usize;
        let limit = query.limit.unwrap_or(u32::MAX) as usize;
        let items = all.into_iter().skip(offset).take(limit).collect();
        Ok(Page { items, total })
    }
}

/// Transfer persistence: store and query committed transfers.
#[async_trait]
pub trait TransferStore: Send + Sync {
    /// Fetch a transfer record by its content-addressed id.
    async fn get_transfer(&self, id: &EnvelopeId) -> Result<Option<EnvelopeRecord>, StoreError>;
    /// Persist a transfer record if absent (idempotent) and index it under every
    /// account in `involved` (both created and consumed owners — the caller
    /// supplies the set so storage computes nothing). A dumb instruction:
    /// returns **1** if the transfer row was newly inserted, **0** if it already
    /// existed. The caller decides what `0` means.
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
    async fn query_transfers(
        &self,
        query: &TransferQuery,
    ) -> Result<Page<EnvelopeRecord>, StoreError> {
        // Default in-memory implementation
        let all = if let Some(account) = query.account {
            self.get_transfers_for_account(account, query.sub).await?
        } else {
            return Err(StoreError::Internal(
                "query_transfers requires account filter in default implementation".into(),
            ));
        };

        let filtered: Vec<EnvelopeRecord> = all
            .into_iter()
            .filter(|r| {
                if let Some(from) = query.from_ts
                    && r.created_at < from
                {
                    return false;
                }
                if let Some(to) = query.to_ts
                    && r.created_at >= to
                {
                    return false;
                }
                if let Some(book) = query.book
                    && r.envelope.book() != book
                {
                    return false;
                }
                true
            })
            .collect();

        let total = filtered.len() as u64;
        let offset = query.offset.unwrap_or(0) as usize;
        let limit = query.limit.unwrap_or(u32::MAX) as usize;
        let items = filtered.into_iter().skip(offset).take(limit).collect();

        Ok(Page { items, total })
    }
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

// ---------------------------------------------------------------------------
// Composite trait
// ---------------------------------------------------------------------------

/// Async storage abstraction composing all sub-traits.
pub trait Store:
    AccountStore + PostingStore + TransferStore + SagaStore + EventStore + BookStore
{
}

impl<T: AccountStore + PostingStore + TransferStore + SagaStore + EventStore + BookStore> Store
    for T
{
}
