//! Storage abstraction separating the pure decision logic from IO.
//!
//! The [`Store`] trait composes four focused sub-traits:
//! - [`AccountStore`] — account CRUD and versioning
//! - [`PostingStore`] — posting reads and lifecycle transitions
//! - [`TransferStore`] — transfer persistence and queries
//! - [`SagaStore`] — saga state for crash recovery

use async_trait::async_trait;
use kuatia_types::{
    Account, AccountId, AssetId, Envelope, EnvelopeId, Book, BookId, Posting, PostingId,
    PostingStatus, Receipt,
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
    /// Reserve postings: Active → PendingInactive.
    /// Atomic: if any posting is not Active, the entire batch fails.
    async fn reserve_postings(&self, ids: &[PostingId]) -> Result<(), StoreError>;
    /// Release postings back from reservation.
    /// - PendingInactive → Active
    /// - Active → no-op (already released)
    /// - Inactive → fail (void posting cannot be released)
    /// Atomic: if any posting is Inactive, the entire batch fails.
    async fn release_postings(&self, ids: &[PostingId]) -> Result<(), StoreError>;
    /// Deactivate postings and insert newly created postings.
    async fn finalize_postings(
        &self,
        deactivate: &[PostingId],
        create: &[Posting],
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
    /// Persist a committed transfer and its receipt.
    async fn store_transfer(&self, record: EnvelopeRecord) -> Result<(), StoreError>;
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
