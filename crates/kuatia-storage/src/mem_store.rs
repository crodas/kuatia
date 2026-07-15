//! In-memory store for tests and single-process embeddings.
//!
//! Accounts are stored as append-only version logs keyed by `AccountId`.

use async_trait::async_trait;
use std::collections::HashMap;
use tokio::sync::RwLock;

use kuatia_types::autoid::AutoId;
use kuatia_types::{
    Account, AccountId, AssetId, Book, BookId, EnvelopeId, Posting, PostingFilter, PostingId,
    PostingState, ReservationId,
};

use crate::error::StoreError;
use crate::events::{EventStore, LedgerEvent};
use crate::store::{
    AccountStore, BookStore, EnvelopeRecord, PostingStore, SagaStore, TransferStore,
};

/// Postings held as an immutable record table plus two index maps that carry
/// full row copies of the live set, so a live-set read never merges back to the
/// immutable record. Kept under one lock so the reserve claim (remove from
/// active + insert into reserved) is atomic.
#[derive(Default)]
struct PostingTables {
    /// The append-only record of every posting; never mutated or removed.
    immutable: HashMap<PostingId, Posting>,
    /// Full copies of spendable postings.
    active: HashMap<PostingId, Posting>,
    /// Full copies of reserved postings, each with its owning reservation.
    reserved: HashMap<PostingId, (Posting, ReservationId)>,
}

/// In-memory [`Store`](crate::store::Store) implementation backed by `RwLock<HashMap>`.
pub struct InMemoryStore {
    postings: RwLock<PostingTables>,
    accounts: RwLock<HashMap<AccountId, Vec<Account>>>,
    transfers: RwLock<HashMap<EnvelopeId, EnvelopeRecord>>,
    /// Transfer-participation index: the `involved` set the caller passed to
    /// `store_transfer`, mirroring the SQL `transfer_accounts` table so both
    /// backends resolve `get_transfers_for_account` from the same instruction.
    transfer_accounts: RwLock<HashMap<EnvelopeId, Vec<AccountId>>>,
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
            postings: RwLock::new(PostingTables::default()),
            accounts: RwLock::new(HashMap::new()),
            transfers: RwLock::new(HashMap::new()),
            transfer_accounts: RwLock::new(HashMap::new()),
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
                .immutable
                .get(id)
                .ok_or_else(|| StoreError::NotFound(format!("posting {id:?}")))?;
            result.push(posting.clone());
        }
        Ok(result)
    }

    async fn get_postings_by_account(
        &self,
        id: i64,
        sub: Option<i64>,
        asset: Option<&AssetId>,
        filter: PostingFilter,
    ) -> Result<Vec<Posting>, StoreError> {
        let postings = self.postings.read().await;
        let matches = |p: &&Posting| {
            p.owner.id == id
                && sub.is_none_or(|s| p.owner.sub == s)
                && asset.is_none_or(|a| p.asset == *a)
        };
        // Read from the relevant set directly — no merge back to the immutable
        // record. `Live` is the two disjoint live sets chained together.
        let reserved = || postings.reserved.values().map(|(p, _)| p);
        let mut result: Vec<Posting> = match filter {
            PostingFilter::Active => postings.active.values().filter(matches).cloned().collect(),
            PostingFilter::Reserved => reserved().filter(matches).cloned().collect(),
            PostingFilter::Live => postings
                .active
                .values()
                .chain(reserved())
                .filter(matches)
                .cloned()
                .collect(),
            PostingFilter::All => postings
                .immutable
                .values()
                .filter(matches)
                .cloned()
                .collect(),
        };
        // Deterministic order by the posting id, matching the SQL backend and
        // `query_postings`; the maps above iterate in an unspecified order.
        result.sort_by_key(|p| p.id);
        Ok(result)
    }

    async fn get_posting_states(&self, ids: &[PostingId]) -> Result<Vec<PostingState>, StoreError> {
        let postings = self.postings.read().await;
        Ok(ids
            .iter()
            .map(|id| {
                if postings.active.contains_key(id) {
                    PostingState::Active
                } else if let Some((_, rid)) = postings.reserved.get(id) {
                    PostingState::Reserved(*rid)
                } else if postings.immutable.contains_key(id) {
                    PostingState::Spent
                } else {
                    PostingState::Missing
                }
            })
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
            // Removing the active copy is the atomic claim: only one caller can
            // remove a given id, and only then is the copy moved to the reserved
            // set (carrying its data, so reserved reads never merge).
            if let Some(p) = postings.active.remove(id) {
                postings.reserved.insert(*id, (p, reservation));
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
            if postings.reserved.get(id).map(|(_, r)| *r) == Some(reservation) {
                if let Some((p, _)) = postings.reserved.remove(id) {
                    postings.active.insert(*id, p);
                    released += 1;
                }
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
            let removed = match reservation {
                None => postings.active.remove(id).is_some(),
                Some(rid) => {
                    if postings.reserved.get(id).map(|(_, r)| *r) == Some(rid) {
                        postings.reserved.remove(id);
                        true
                    } else {
                        false
                    }
                }
            };
            if removed {
                changed += 1;
            }
        }
        Ok(changed)
    }

    async fn insert_postings(&self, postings: &[Posting]) -> Result<u64, StoreError> {
        let mut store = self.postings.write().await;
        let mut inserted: u64 = 0;
        for posting in postings {
            if let std::collections::hash_map::Entry::Vacant(e) = store.immutable.entry(posting.id)
            {
                e.insert(posting.clone());
                // Only newly-inserted postings are activated; a since-spent
                // posting is not re-activated on a replayed insert. The active
                // set carries a full copy so spendable reads never merge.
                store.active.insert(posting.id, posting.clone());
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
        involved: &[AccountId],
    ) -> Result<u64, StoreError> {
        // Index the transfer under exactly the accounts the caller supplied,
        // deriving nothing — the same instruction the SQL backend follows into
        // its `transfer_accounts` table. Idempotent: a replay writes the same
        // set. The returned count reflects only whether the transfer row was
        // newly inserted; the caller decides what `0` means.
        let tid = record.receipt.transfer_id;
        // Lock order transfers → transfer_accounts, matching every other reader.
        let mut transfers = self.transfers.write().await;
        let mut transfer_accounts = self.transfer_accounts.write().await;
        transfer_accounts.insert(tid, involved.to_vec());
        if transfers.contains_key(&tid) {
            return Ok(0);
        }
        transfers.insert(tid, record);
        Ok(1)
    }

    async fn get_transfers_for_account(
        &self,
        id: i64,
        sub: Option<i64>,
    ) -> Result<Vec<EnvelopeRecord>, StoreError> {
        // Resolve participation from the `involved` index, not from postings, so
        // this backend answers exactly what `store_transfer` was told — matching
        // the SQL `transfer_accounts` join. Lock order transfers → index.
        let transfers = self.transfers.read().await;
        let transfer_accounts = self.transfer_accounts.read().await;
        let matches = |owner: &AccountId| owner.id == id && sub.is_none_or(|s| owner.sub == s);
        let mut result: Vec<EnvelopeRecord> = transfer_accounts
            .iter()
            .filter(|(_, accounts)| accounts.iter().any(matches))
            .filter_map(|(tid, _)| transfers.get(tid).cloned())
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
