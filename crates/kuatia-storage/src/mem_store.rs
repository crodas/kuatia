//! In-memory store for tests and single-process embeddings.
//!
//! Accounts are stored as append-only version logs keyed by `AccountId`.

use async_trait::async_trait;
use std::collections::HashMap;
use tokio::sync::RwLock;

use kuatia_types::autoid::AutoId;
use kuatia_types::{Account, AccountId, AssetId, EnvelopeId, Posting, PostingId, PostingStatus};

use crate::error::StoreError;
use crate::events::{EventStore, LedgerEvent};
use crate::store::{AccountStore, EnvelopeRecord, PostingStore, SagaStore, TransferStore};

/// In-memory [`Store`](crate::store::Store) implementation backed by `RwLock<HashMap>`.
pub struct InMemoryStore {
    postings: RwLock<HashMap<PostingId, Posting>>,
    accounts: RwLock<HashMap<AccountId, Vec<Account>>>,
    transfers: RwLock<HashMap<EnvelopeId, EnvelopeRecord>>,
    sagas: RwLock<HashMap<i64, Vec<u8>>>,
    events: RwLock<Vec<LedgerEvent>>,
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

    async fn reserve_postings(&self, ids: &[PostingId]) -> Result<(), StoreError> {
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
            postings
                .get_mut(id)
                .ok_or_else(|| StoreError::NotFound(format!("posting {id:?}")))?
                .status = PostingStatus::PendingInactive;
        }
        Ok(())
    }

    async fn release_postings(&self, ids: &[PostingId]) -> Result<(), StoreError> {
        let mut postings = self.postings.write().await;
        for id in ids {
            let posting = postings
                .get(id)
                .ok_or_else(|| StoreError::NotFound(format!("posting {id:?}")))?;
            if posting.status == PostingStatus::Inactive {
                return Err(StoreError::PostingInactive(*id));
            }
        }
        for id in ids {
            let posting = postings
                .get_mut(id)
                .ok_or_else(|| StoreError::NotFound(format!("posting {id:?}")))?;
            if posting.status == PostingStatus::PendingInactive {
                posting.status = PostingStatus::Active;
            }
        }
        Ok(())
    }

    async fn finalize_postings(
        &self,
        deactivate: &[PostingId],
        create: &[Posting],
    ) -> Result<(), StoreError> {
        let mut postings = self.postings.write().await;
        for pid in deactivate {
            if let Some(p) = postings.get_mut(pid) {
                p.status = PostingStatus::Inactive;
            }
        }
        for posting in create {
            postings.insert(posting.id, posting.clone());
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

    async fn store_transfer(&self, record: EnvelopeRecord) -> Result<(), StoreError> {
        let mut transfers = self.transfers.write().await;
        transfers.insert(record.receipt.transfer_id, record);
        Ok(())
    }

    async fn get_transfers_for_account(
        &self,
        account: &AccountId,
    ) -> Result<Vec<EnvelopeRecord>, StoreError> {
        let transfers = self.transfers.read().await;
        let postings = self.postings.read().await;
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
