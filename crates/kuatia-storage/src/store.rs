//! Storage abstraction separating the pure decision logic from IO.
//!
//! The [`Store`] trait composes six focused sub-traits:
//! - [`AccountStore`] — account CRUD and versioning
//! - [`PostingStore`] — posting reads and lifecycle transitions
//! - [`TransferStore`] — transfer persistence and queries
//! - [`SagaStore`] — saga state for crash recovery
//! - [`EventStore`](crate::events::EventStore) — the ledger event log
//! - [`BookStore`] — book persistence
//! - [`CommitStore`] — the single atomic commit boundary

use async_trait::async_trait;
use kuatia_types::{
    Account, AccountId, AssetId, Book, BookId, Cent, Envelope, EnvelopeId, Posting, PostingId,
    PostingStatus, Receipt, ReservationId,
};

use crate::error::StoreError;
use crate::events::{EventStore, LedgerEvent};

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

/// Everything one atomic commit must persist together. Carries decomposed
/// primitives (not `kuatia_core::Plan`) so this crate need not depend on the
/// pure-core crate.
pub struct CommitRequest<'a> {
    /// Consumed postings to mark `Inactive`.
    pub deactivate: &'a [PostingId],
    /// New postings to insert (already `Active`, from the validated plan).
    pub create: &'a [Posting],
    /// `(account, asset, expected_balance)` guards to verify before mutating —
    /// a mismatch means a concurrent transfer moved the balance ([`StoreError::Conflict`]).
    pub cas_guards: &'a [(AccountId, AssetId, Cent)],
    /// `(account, expected_version)` guards re-checked atomically at commit — a
    /// mismatch means a concurrent lifecycle mutation (freeze/unfreeze/close)
    /// bumped the account version after validation ([`StoreError::VersionConflict`]).
    pub account_guards: &'a [(AccountId, u64)],
    /// Reservation authorizing consumption of `deactivate`.
    /// - `None` — raw path: the postings must be `Active`.
    /// - `Some(rid)` — saga path: the postings must be `PendingInactive` owned by `rid`.
    pub reservation: Option<ReservationId>,
    /// The transfer record to persist.
    pub record: EnvelopeRecord,
    /// Events to append within the same transaction (e.g. `TransferCommitted`).
    pub events: &'a [LedgerEvent],
}

/// Pagination and filtering parameters for posting queries.
#[derive(Debug, Clone)]
pub struct PostingQuery {
    /// Filter to postings owned by this account.
    pub account: AccountId,
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
    /// Filter to transfers involving this account.
    pub account: Option<AccountId>,
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
    /// Return postings owned by an account, optionally filtered by asset and/or status.
    async fn get_postings_by_account(
        &self,
        account: &AccountId,
        asset: Option<&AssetId>,
        status: Option<PostingStatus>,
    ) -> Result<Vec<Posting>, StoreError>;
    /// Reserve postings: Active → PendingInactive, stamping `reservation` as the
    /// owner token. Atomic: if any posting is not Active, the entire batch fails.
    async fn reserve_postings(
        &self,
        ids: &[PostingId],
        reservation: ReservationId,
    ) -> Result<(), StoreError>;
    /// Release postings reserved under `reservation`, back from reservation.
    /// - PendingInactive owned by `reservation` → Active (clears the owner)
    /// - PendingInactive owned by a different reservation → fail ([`StoreError::ReservationMismatch`])
    /// - Active → no-op (already released)
    /// - Inactive → fail (void posting cannot be released)
    /// Atomic: if any posting fails its check, the entire batch fails.
    async fn release_postings(
        &self,
        ids: &[PostingId],
        reservation: ReservationId,
    ) -> Result<(), StoreError>;

    /// Query postings with filtering and pagination.
    async fn query_postings(&self, query: &PostingQuery) -> Result<Page<Posting>, StoreError> {
        let all = self
            .get_postings_by_account(
                &query.account,
                query.asset.as_ref(),
                query.status,
            )
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
    /// Return all transfers involving the given account.
    async fn get_transfers_for_account(
        &self,
        account: &AccountId,
    ) -> Result<Vec<EnvelopeRecord>, StoreError>;

    /// Query transfers with filtering and pagination.
    async fn query_transfers(
        &self,
        query: &TransferQuery,
    ) -> Result<Page<EnvelopeRecord>, StoreError> {
        // Default in-memory implementation
        let all = if let Some(ref account) = query.account {
            self.get_transfers_for_account(account).await?
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

/// The single atomic commit boundary — the one place ledger state changes.
#[async_trait]
pub trait CommitStore: Send + Sync {
    /// Apply a validated transfer atomically: enforce CAS guards, authorize and
    /// deactivate consumed postings, insert created postings, persist the
    /// transfer record (indexed by **both** created and consumed account owners),
    /// and append the events — all in one critical section.
    ///
    /// Idempotent on the transfer id: if already committed, returns `Ok(())`.
    /// Returns [`StoreError::Conflict`] (retryable) if a guard balance changed,
    /// or [`StoreError::ReservationMismatch`] if a consumed posting is not owned
    /// as `req.reservation` requires.
    async fn commit_transfer(&self, req: CommitRequest<'_>) -> Result<(), StoreError>;
}

// ---------------------------------------------------------------------------
// Composite trait
// ---------------------------------------------------------------------------

/// Async storage abstraction composing all sub-traits.
pub trait Store:
    AccountStore + PostingStore + TransferStore + SagaStore + EventStore + BookStore + CommitStore
{
}

impl<
    T: AccountStore
        + PostingStore
        + TransferStore
        + SagaStore
        + EventStore
        + BookStore
        + CommitStore,
> Store for T
{
}
