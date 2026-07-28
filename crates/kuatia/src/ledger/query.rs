//! Read-only queries and book CRUD.
//!
//! These methods delegate to the [`Store`] and relabel `StoreError` as
//! [`LedgerError`] so callers get one error type across the whole API. The
//! underlying store is reachable via [`Ledger::store`] for callers that want the
//! raw storage error instead.

use kuatia_core::{
    Account, AccountId, Book, BookId, Posting, PostingFilter, PostingId, PostingState,
};
use kuatia_storage::events::LedgerEvent;

use super::Ledger;
use crate::error::LedgerError;
use crate::store::{EnvelopeRecord, Page, PostingQuery, TransferQuery};

impl Ledger {
    /// List all accounts (latest version of each).
    pub async fn list_accounts(&self) -> Result<Vec<Account>, LedgerError> {
        Ok(self.store.list_accounts().await?)
    }

    /// Fetch a single account by id.
    pub async fn get_account(&self, id: &AccountId) -> Result<Account, LedgerError> {
        self.store
            .get_account(id)
            .await
            .map_err(|_| LedgerError::AccountNotFound(*id))
    }

    /// Return all transfers involving the given account (exact subaccount).
    pub async fn history(&self, account: &AccountId) -> Result<Vec<EnvelopeRecord>, LedgerError> {
        Ok(self
            .store
            .get_transfers_for_account(account.id, Some(account.sub))
            .await?)
    }

    /// Query transfers with filtering and pagination.
    pub async fn query_transfers(
        &self,
        query: &TransferQuery,
    ) -> Result<Page<EnvelopeRecord>, LedgerError> {
        Ok(self.store.query_transfers(query).await?)
    }

    /// Return all postings (any state) for the given account.
    pub async fn postings(&self, account: &AccountId) -> Result<Vec<Posting>, LedgerError> {
        Ok(self
            .store
            .get_postings_by_account(account.id, Some(account.sub), None, PostingFilter::All)
            .await?)
    }

    /// Return all postings for the given account paired with their derived
    /// lifecycle state (active, reserved, or spent).
    pub async fn postings_with_state(
        &self,
        account: &AccountId,
    ) -> Result<Vec<(Posting, PostingState)>, LedgerError> {
        let postings = self.postings(account).await?;
        let ids: Vec<PostingId> = postings.iter().map(|p| p.id).collect();
        let states = self.store.get_posting_states(&ids).await?;
        Ok(postings.into_iter().zip(states).collect())
    }

    /// Query postings with filtering and pagination.
    pub async fn query_postings(&self, query: &PostingQuery) -> Result<Page<Posting>, LedgerError> {
        Ok(self.store.query_postings(query).await?)
    }

    /// Return the full version history for an account.
    pub async fn account_history(&self, id: &AccountId) -> Result<Vec<Account>, LedgerError> {
        Ok(self.store.get_account_history(id).await?)
    }

    /// Create a new book.
    pub async fn create_book(&self, book: Book) -> Result<(), LedgerError> {
        let id = book.id;
        if self.store.create_book(book).await? == 0 {
            return Err(LedgerError::BookAlreadyExists(id));
        }
        Ok(())
    }

    /// Fetch a book by id.
    pub async fn get_book(&self, id: &BookId) -> Result<Book, LedgerError> {
        Ok(self.store.get_book(id).await?)
    }

    /// List all books.
    pub async fn list_books(&self) -> Result<Vec<Book>, LedgerError> {
        Ok(self.store.list_books().await?)
    }

    /// Query ledger events after a given sequence number.
    pub async fn get_events_since(
        &self,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<LedgerEvent>, LedgerError> {
        Ok(self.store.get_events_since(after_seq, limit).await?)
    }
}
