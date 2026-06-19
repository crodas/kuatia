//! In-memory store for tests and single-process embeddings.
//!
//! Accounts are stored as append-only version logs keyed by `AccountId`.

use async_trait::async_trait;
use std::collections::HashMap;
use tokio::sync::RwLock;

use kuatia_types::autoid::AutoId;
use kuatia_types::{
    Account, AccountId, AssetId, Book, BookId, EnvelopeId, Posting, PostingId, PostingStatus,
    ReservationId,
};

use crate::error::StoreError;
use crate::events::{EventStore, LedgerEvent};
use crate::store::{
    AccountStore, BookStore, EnvelopeRecord, PostingStore, SagaStore, TransferStore,
};

/// In-memory [`Store`](crate::store::Store) implementation backed by `RwLock<HashMap>`.
pub struct InMemoryStore {
    postings: RwLock<HashMap<PostingId, Posting>>,
    accounts: RwLock<HashMap<AccountId, Vec<Account>>>,
    transfers: RwLock<HashMap<EnvelopeId, EnvelopeRecord>>,
    sagas: RwLock<HashMap<i64, Vec<u8>>>,
    events: RwLock<Vec<LedgerEvent>>,
    books: RwLock<HashMap<BookId, Book>>,
    autoid: AutoId,
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryStore {
    /// Create an empty in-memory store.
    pub fn new() -> Self {
        Self {
            postings: RwLock::new(HashMap::new()),
            accounts: RwLock::new(HashMap::new()),
            transfers: RwLock::new(HashMap::new()),
            sagas: RwLock::new(HashMap::new()),
            events: RwLock::new(Vec::new()),
            books: RwLock::new(HashMap::new()),
            autoid: AutoId::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// AccountStore
// ---------------------------------------------------------------------------

#[async_trait]
impl AccountStore for InMemoryStore {
    async fn get_account(&self, id: &AccountId) -> Result<Account, StoreError> {
        let accounts = self.accounts.read().await;
        accounts
            .get(id)
            .and_then(|v| v.last())
            .cloned()
            .ok_or_else(|| StoreError::NotFound(format!("account {id:?}")))
    }

    async fn get_accounts(&self, ids: &[AccountId]) -> Result<Vec<Account>, StoreError> {
        let accounts = self.accounts.read().await;
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            let account = accounts
                .get(id)
                .and_then(|v| v.last())
                .cloned()
                .ok_or_else(|| StoreError::NotFound(format!("account {id:?}")))?;
            result.push(account);
        }
        Ok(result)
    }

    async fn create_account(&self, account: Account) -> Result<(), StoreError> {
        let id = account.id;
        let mut accounts = self.accounts.write().await;
        if accounts.contains_key(&id) {
            return Err(StoreError::AlreadyExists(format!("account {id:?}")));
        }
        accounts.insert(id, vec![account]);
        Ok(())
    }

    async fn append_account_version(&self, account: Account) -> Result<(), StoreError> {
        let id = account.id;
        let mut accounts = self.accounts.write().await;
        let versions = accounts
            .get_mut(&id)
            .ok_or_else(|| StoreError::NotFound(format!("account {id:?}")))?;
        let current_version = versions.last().map(|a| a.version).unwrap_or(0);
        let expected = current_version
            .checked_add(1)
            .ok_or_else(|| StoreError::Internal("account version overflow".to_string()))?;
        if account.version != expected {
            return Err(StoreError::VersionConflict {
                account: account.id,
                expected,
                actual: account.version,
            });
        }
        versions.push(account);
        Ok(())
    }

    async fn get_account_history(&self, id: &AccountId) -> Result<Vec<Account>, StoreError> {
        let accounts = self.accounts.read().await;
        accounts
            .get(id)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(format!("account {id:?}")))
    }

    async fn list_accounts(&self) -> Result<Vec<Account>, StoreError> {
        let accounts = self.accounts.read().await;
        Ok(accounts
            .values()
            .filter_map(|v| v.last().cloned())
            .collect())
    }
}

// ---------------------------------------------------------------------------
// PostingStore
// ---------------------------------------------------------------------------

#[async_trait]
impl PostingStore for InMemoryStore {
    async fn get_postings(&self, ids: &[PostingId]) -> Result<Vec<Posting>, StoreError> {
        let postings = self.postings.read().await;
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            let posting = postings
                .get(id)
                .ok_or_else(|| StoreError::NotFound(format!("posting {id:?}")))?;
            result.push(posting.clone());
        }
        Ok(result)
    }

    async fn get_postings_by_account(
        &self,
        account: &AccountId,
        asset: Option<&AssetId>,
        status: Option<PostingStatus>,
    ) -> Result<Vec<Posting>, StoreError> {
        let postings = self.postings.read().await;
        Ok(postings
            .values()
            .filter(|p| {
                p.owner == *account
                    && asset.is_none_or(|a| p.asset == *a)
                    && status.is_none_or(|s| p.status == s)
            })
            .cloned()
            .collect())
    }

    async fn reserve_postings(
        &self,
        ids: &[PostingId],
        reservation: ReservationId,
    ) -> Result<u64, StoreError> {
        let mut postings = self.postings.write().await;
        let mut reserved: u64 = 0;
        for id in ids {
            let Some(posting) = postings.get_mut(id) else {
                continue; // dumb: a missing row just doesn't count
            };
            if posting.status == PostingStatus::Active {
                posting.status = PostingStatus::PendingInactive;
                posting.reservation = Some(reservation);
                reserved += 1;
            }
        }
        Ok(reserved)
    }

    async fn release_postings(
        &self,
        ids: &[PostingId],
        reservation: ReservationId,
    ) -> Result<u64, StoreError> {
        let mut postings = self.postings.write().await;
        let mut released: u64 = 0;
        for id in ids {
            let Some(posting) = postings.get_mut(id) else {
                continue;
            };
            if posting.status == PostingStatus::PendingInactive
                && posting.reservation == Some(reservation)
            {
                posting.status = PostingStatus::Active;
                posting.reservation = None;
                released += 1;
            }
        }
        Ok(released)
    }

    async fn deactivate_postings(
        &self,
        ids: &[PostingId],
        reservation: Option<ReservationId>,
    ) -> Result<u64, StoreError> {
        let mut postings = self.postings.write().await;
        let mut changed: u64 = 0;
        for id in ids {
            let Some(posting) = postings.get_mut(id) else {
                continue; // dumb: a missing row just doesn't count
            };
            let matches = match reservation {
                None => posting.status == PostingStatus::Active,
                Some(rid) => {
                    posting.status == PostingStatus::PendingInactive
                        && posting.reservation == Some(rid)
                }
            };
            if matches {
                posting.status = PostingStatus::Inactive;
                posting.reservation = None;
                changed += 1;
            }
        }
        Ok(changed)
    }

    async fn insert_postings(&self, postings: &[Posting]) -> Result<u64, StoreError> {
        let mut store = self.postings.write().await;
        let mut inserted: u64 = 0;
        for posting in postings {
            if let std::collections::hash_map::Entry::Vacant(e) = store.entry(posting.id) {
                e.insert(posting.clone());
                inserted += 1;
            }
        }
        Ok(inserted)
    }
}

// ---------------------------------------------------------------------------
// TransferStore
// ---------------------------------------------------------------------------

#[async_trait]
impl TransferStore for InMemoryStore {
    async fn get_transfer(&self, id: &EnvelopeId) -> Result<Option<EnvelopeRecord>, StoreError> {
        let transfers = self.transfers.read().await;
        Ok(transfers.get(id).cloned())
    }

    async fn store_transfer(
        &self,
        record: EnvelopeRecord,
        _involved: &[AccountId],
    ) -> Result<u64, StoreError> {
        // `_involved` is ignored here: `get_transfers_for_account` derives the
        // involved accounts from the stored envelope (creates owners + consumed
        // posting owners), which matches the set the caller passes. The SQL
        // backend instead persists `involved` into its `transfer_accounts` index.
        let mut transfers = self.transfers.write().await;
        if transfers.contains_key(&record.receipt.transfer_id) {
            return Ok(0);
        }
        transfers.insert(record.receipt.transfer_id, record);
        Ok(1)
    }

    async fn get_transfers_for_account(
        &self,
        account: &AccountId,
    ) -> Result<Vec<EnvelopeRecord>, StoreError> {
        // Acquire postings → transfers in a consistent order to avoid an AB–BA
        // deadlock with any reader that takes both.
        let postings = self.postings.read().await;
        let transfers = self.transfers.read().await;
        let mut result: Vec<EnvelopeRecord> = transfers
            .values()
            .filter(|record| {
                record
                    .envelope
                    .creates()
                    .iter()
                    .any(|np| np.owner == *account)
                    || record
                        .envelope
                        .consumes()
                        .iter()
                        .any(|pid| postings.get(pid).is_some_and(|p| p.owner == *account))
            })
            .cloned()
            .collect();
        result.sort_by_key(|r| r.created_at);
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// SagaStore
// ---------------------------------------------------------------------------

#[async_trait]
impl SagaStore for InMemoryStore {
    async fn save_saga(&self, id: &i64, data: Vec<u8>) -> Result<(), StoreError> {
        let mut sagas = self.sagas.write().await;
        sagas.insert(*id, data);
        Ok(())
    }

    async fn list_pending_sagas(&self) -> Result<Vec<(i64, Vec<u8>)>, StoreError> {
        let sagas = self.sagas.read().await;
        Ok(sagas.iter().map(|(k, v)| (*k, v.clone())).collect())
    }

    async fn delete_saga(&self, id: &i64) -> Result<(), StoreError> {
        let mut sagas = self.sagas.write().await;
        sagas.remove(id);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// EventStore
// ---------------------------------------------------------------------------

#[async_trait]
impl EventStore for InMemoryStore {
    async fn append_event(&self, event: &LedgerEvent) -> Result<u64, StoreError> {
        let mut events = self.events.write().await;
        // Idempotent on the dedup key: a replayed transfer event returns the
        // existing seq instead of inserting a duplicate.
        if let Some(key) = crate::events::event_dedup_key(&event.kind)
            && let Some(existing) = events
                .iter()
                .find(|e| crate::events::event_dedup_key(&e.kind) == Some(key))
        {
            return Ok(existing.seq);
        }
        let seq = self.autoid.next() as u64;
        events.push(LedgerEvent {
            seq,
            timestamp: event.timestamp,
            kind: event.kind.clone(),
        });
        Ok(seq)
    }

    async fn get_events_since(
        &self,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<LedgerEvent>, StoreError> {
        let events = self.events.read().await;
        Ok(events
            .iter()
            .filter(|e| e.seq > after_seq)
            .take(limit as usize)
            .cloned()
            .collect())
    }
}

#[async_trait]
impl BookStore for InMemoryStore {
    async fn create_book(&self, book: Book) -> Result<(), StoreError> {
        let mut books = self.books.write().await;
        if books.contains_key(&book.id) {
            return Err(StoreError::AlreadyExists(format!("book {:?}", book.id)));
        }
        books.insert(book.id, book);
        Ok(())
    }

    async fn get_book(&self, id: &BookId) -> Result<Book, StoreError> {
        let books = self.books.read().await;
        books
            .get(id)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(format!("book {id:?}")))
    }

    async fn list_books(&self) -> Result<Vec<Book>, StoreError> {
        let books = self.books.read().await;
        Ok(books.values().cloned().collect())
    }
}
