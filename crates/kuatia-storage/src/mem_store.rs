//! In-memory store for tests and single-process embeddings.
//!
//! Accounts are stored as append-only version logs keyed by `AccountId`.

use async_trait::async_trait;
use std::collections::HashMap;
use tokio::sync::RwLock;

use kuatia_types::autoid::AutoId;
use kuatia_types::{
    Account, AccountId, AssetId, Book, BookId, Cent, EnvelopeId, Posting, PostingId, PostingStatus,
    ReservationId,
};

use crate::error::StoreError;
use crate::events::{EventStore, LedgerEvent};
use crate::store::{
    AccountStore, BookStore, CommitRequest, CommitStore, EnvelopeRecord, PostingStore, SagaStore,
    TransferStore,
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
    ) -> Result<(), StoreError> {
        let mut postings = self.postings.write().await;
        for id in ids {
            let posting = postings
                .get(id)
                .ok_or_else(|| StoreError::NotFound(format!("posting {id:?}")))?;
            if posting.status != PostingStatus::Active {
                return Err(StoreError::PostingNotActive(*id));
            }
        }
        for id in ids {
            let posting = postings
                .get_mut(id)
                .ok_or_else(|| StoreError::NotFound(format!("posting {id:?}")))?;
            posting.status = PostingStatus::PendingInactive;
            posting.reservation = Some(reservation);
        }
        Ok(())
    }

    async fn release_postings(
        &self,
        ids: &[PostingId],
        reservation: ReservationId,
    ) -> Result<(), StoreError> {
        let mut postings = self.postings.write().await;
        for id in ids {
            let posting = postings
                .get(id)
                .ok_or_else(|| StoreError::NotFound(format!("posting {id:?}")))?;
            match posting.status {
                PostingStatus::Inactive => return Err(StoreError::PostingInactive(*id)),
                PostingStatus::PendingInactive if posting.reservation != Some(reservation) => {
                    return Err(StoreError::ReservationMismatch(*id));
                }
                _ => {}
            }
        }
        for id in ids {
            let posting = postings
                .get_mut(id)
                .ok_or_else(|| StoreError::NotFound(format!("posting {id:?}")))?;
            if posting.status == PostingStatus::PendingInactive {
                posting.status = PostingStatus::Active;
                posting.reservation = None;
            }
        }
        Ok(())
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

    async fn get_transfers_for_account(
        &self,
        account: &AccountId,
    ) -> Result<Vec<EnvelopeRecord>, StoreError> {
        // Lock order postings → transfers must match `commit_transfer` to avoid
        // an AB–BA deadlock.
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
        let seq = self.autoid.next() as u64;
        let mut events = self.events.write().await;
        let stored = LedgerEvent {
            seq,
            timestamp: event.timestamp,
            kind: event.kind.clone(),
        };
        events.push(stored);
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
            return Err(StoreError::AlreadyExists(format!(
                "book {:?}",
                book.id
            )));
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

// ---------------------------------------------------------------------------
// CommitStore
// ---------------------------------------------------------------------------

#[async_trait]
impl CommitStore for InMemoryStore {
    async fn commit_transfer(&self, req: CommitRequest<'_>) -> Result<(), StoreError> {
        // Lock order accounts → postings → transfers → events; every reader that
        // takes more than one of these must follow the same order. Holding the
        // accounts read lock for the whole commit keeps the version guard (step
        // 2b) atomic against a concurrent lifecycle mutation.
        let accounts = self.accounts.read().await;
        let mut postings = self.postings.write().await;
        let mut transfers = self.transfers.write().await;
        let mut events = self.events.write().await;

        let tid = req.record.receipt.transfer_id;

        // 1. Idempotency: a prior attempt already committed this transfer.
        if transfers.contains_key(&tid) {
            return Ok(());
        }

        // 2. CAS guards — recompute each balance (Σ non-Inactive postings) before
        //    any mutation, matching how validation snapshotted it.
        for (account, asset, expected) in req.cas_guards {
            let balance = Cent::checked_sum(
                postings
                    .values()
                    .filter(|p| {
                        p.owner == *account
                            && p.asset == *asset
                            && p.status != PostingStatus::Inactive
                    })
                    .map(|p| p.value),
            )
            .map_err(|_| StoreError::Internal("balance overflow during cas".into()))?;
            if balance != *expected {
                return Err(StoreError::Conflict {
                    account: *account,
                    asset: *asset,
                });
            }
        }

        // 2b. Account version guards — a concurrent freeze/unfreeze/close bumps
        //     the version, invalidating the snapshot pinned at validation.
        for (account, expected) in req.account_guards {
            let actual = accounts
                .get(account)
                .and_then(|versions| versions.last())
                .map(|a| a.version)
                .ok_or(StoreError::NotFound(format!("account {account:?}")))?;
            if actual != *expected {
                return Err(StoreError::VersionConflict {
                    account: *account,
                    expected: *expected,
                    actual,
                });
            }
        }

        // 3. Authorize every consumed posting against the reservation.
        for pid in req.deactivate {
            let posting = postings
                .get(pid)
                .ok_or(StoreError::ReservationMismatch(*pid))?;
            match req.reservation {
                None => {
                    if posting.status != PostingStatus::Active {
                        return Err(StoreError::ReservationMismatch(*pid));
                    }
                }
                Some(rid) => {
                    if posting.status != PostingStatus::PendingInactive
                        || posting.reservation != Some(rid)
                    {
                        return Err(StoreError::ReservationMismatch(*pid));
                    }
                }
            }
        }

        // 4. Deactivate consumed postings.
        for pid in req.deactivate {
            let posting = postings
                .get_mut(pid)
                .ok_or(StoreError::ReservationMismatch(*pid))?;
            posting.status = PostingStatus::Inactive;
            posting.reservation = None;
        }

        // 5. Insert created postings.
        for posting in req.create {
            postings.insert(posting.id, posting.clone());
        }

        // 6. Persist the transfer record.
        transfers.insert(tid, req.record);

        // 7. Append events in-transaction, assigning sequence numbers.
        for event in req.events {
            let seq = self.autoid.next() as u64;
            events.push(LedgerEvent {
                seq,
                timestamp: event.timestamp,
                kind: event.kind.clone(),
            });
        }

        Ok(())
    }
}
