//! The async ledger resource -- the primary entry point for callers.

use std::collections::HashMap;
use std::sync::Arc;

use legend::{ExecutionResult, legend};
use tracing::instrument;

use kuatia_core::{
    AccountId, AccountPolicy, AccountSnapshotId, AssetId, Book, Cent, DEFAULT_BOOK, Envelope,
    EnvelopeBuilder, EnvelopeId, NewPosting, PlanInput, Posting, PostingId, PostingStatus, Receipt,
    SelectionError, Transfer, account_snapshot_id, envelope_id, select_postings, validate_and_plan,
};

use crate::error::LedgerError;

/// Return the current time as Unix milliseconds.
pub(crate) fn now_millis() -> Result<i64, LedgerError> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| LedgerError::Overflow)?
        .as_millis() as i64)
}
use crate::saga::{
    FinalizeInput, FinalizeTransferStep, LedgerCtx, ReserveInput, ReservePostingsStep, SagaError,
    ValidateInput, ValidateTransferStep,
};
use kuatia_storage::error::StoreError;
use kuatia_storage::events::{LedgerEvent, LedgerEventKind};
use kuatia_storage::store::{EnvelopeRecord, Store};

#[allow(missing_docs)]
mod envelope_saga {
    use super::*;
    legend! {
        EnvelopeSaga<LedgerCtx, SagaError> {
            reserve: ReservePostingsStep,
            validate: ValidateTransferStep,
            finalize: FinalizeTransferStep,
        }
    }
}
use envelope_saga::*;

/// Write-ahead record for an in-flight commit, persisted via `SagaStore` before
/// the saga mutates anything and removed once it reaches a terminal state. On
/// startup [`Ledger::recover`] re-drives any that survive a crash.
#[derive(serde::Serialize, serde::Deserialize)]
struct PendingSaga {
    envelope: Envelope,
    reservation: kuatia_core::ReservationId,
}

/// Async ledger resource composing the commit pipeline.
pub struct Ledger {
    store: Arc<dyn Store>,
}

impl Ledger {
    /// Create a new ledger backed by the given store.
    pub fn new(store: impl Store + 'static) -> Self {
        Self {
            store: Arc::new(store),
        }
    }

    /// Returns a reference to the underlying store.
    pub fn store(&self) -> &dyn Store {
        self.store.as_ref()
    }

    // -----------------------------------------------------------------------
    // Three-piece API: load -> plan -> apply
    // -----------------------------------------------------------------------

    /// Phase 1: load all state needed for validation.
    #[instrument(skip(self, envelope), name = "ledger.load")]
    pub async fn load(&self, envelope: &Envelope) -> Result<LoadedState, LedgerError> {
        let consumed_postings = if envelope.consumes().is_empty() {
            vec![]
        } else {
            self.store.get_postings(envelope.consumes()).await?
        };

        let mut account_ids: Vec<AccountId> = envelope.creates().iter().map(|p| p.owner).collect();
        for p in &consumed_postings {
            account_ids.push(p.owner);
        }
        account_ids.sort();
        account_ids.dedup();

        let account_list = self.store.get_accounts(&account_ids).await?;
        let accounts: HashMap<AccountId, _> = account_list.into_iter().map(|a| (a.id, a)).collect();

        let mut balance_keys: Vec<(AccountId, AssetId)> = Vec::new();
        for p in &consumed_postings {
            balance_keys.push((p.owner, p.asset));
        }
        for np in envelope.creates() {
            balance_keys.push((np.owner, np.asset));
        }
        balance_keys.sort();
        balance_keys.dedup();

        let mut balances = HashMap::new();
        for (account_id, asset_id) in &balance_keys {
            let bal = self.compute_balance(account_id, asset_id).await?;
            balances.insert((*account_id, *asset_id), bal);
        }

        // Load the gating book. A missing named (non-default) book is an error;
        // a missing default book means "unrestricted" (no policy to enforce).
        let book_id = envelope.book();
        let book = match self.store.get_book(&book_id).await {
            Ok(b) => Some(b),
            Err(StoreError::NotFound(_)) if book_id == DEFAULT_BOOK => None,
            Err(StoreError::NotFound(_)) => return Err(LedgerError::BookNotFound(book_id)),
            Err(e) => return Err(e.into()),
        };

        Ok(LoadedState {
            consumed_postings,
            accounts,
            balances,
            book,
        })
    }

    /// Phase 2: run pure validation and produce a plan.
    pub fn plan(
        &self,
        envelope: &Envelope,
        loaded: &LoadedState,
    ) -> Result<kuatia_core::Plan, LedgerError> {
        let input = PlanInput {
            envelope,
            consumed_postings: &loaded.consumed_postings,
            accounts: &loaded.accounts,
            balances: &loaded.balances,
            book: loaded.book.as_ref(),
        };
        Ok(validate_and_plan(input)?)
    }

    // -----------------------------------------------------------------------
    // Resolve: Transfer (intent) -> Envelope (concrete postings)
    // -----------------------------------------------------------------------

    /// Convert a [`Transfer`] intent into a concrete [`Envelope`] by selecting
    /// postings for each movement and computing change.
    ///
    /// Pass 1: create output postings and aggregate net debits per (account, asset).
    /// Pass 2: for each pair with a positive net debit, select postings and compute change.
    #[instrument(skip(self, transfer), name = "ledger.resolve")]
    pub async fn resolve(&self, transfer: &Transfer) -> Result<Envelope, LedgerError> {
        let mut consumes: Vec<PostingId> = Vec::new();
        let mut creates: Vec<NewPosting> = Vec::new();
        let mut net_debits: HashMap<(AccountId, AssetId), Cent> = HashMap::new();

        // Pass 1: output postings + debit aggregation
        for m in &transfer.movements {
            let payer = if m.from != m.to { Some(m.from) } else { None };
            creates.push(NewPosting {
                owner: m.to,
                asset: m.asset,
                value: m.amount,
                payer,
            });
            let entry = net_debits.entry((m.from, m.asset)).or_insert(Cent::ZERO);
            *entry = entry.checked_add(m.amount)?;
        }

        // Pass 2: posting selection for accounts with positive net debit
        for ((account, asset), net_debit) in &net_debits {
            if !net_debit.is_positive() {
                continue;
            }
            let available = self
                .store
                .get_postings_by_account(account, Some(asset), Some(PostingStatus::Active))
                .await?;
            let total_positive = Cent::checked_sum(
                available.iter().filter(|p| p.value.is_positive()).map(|p| p.value),
            )?;

            if total_positive >= *net_debit {
                // Enough positive postings: select a subset and compute change.
                let selected = select_postings(&available, *asset, *net_debit)?;
                let consumed_sum = Cent::checked_sum(
                    available
                        .iter()
                        .filter(|p| selected.contains(&p.id))
                        .map(|p| p.value),
                )?;
                let change = consumed_sum.checked_sub(*net_debit)?;

                consumes.extend_from_slice(&selected);
                if change.is_positive() {
                    creates.push(NewPosting {
                        owner: *account,
                        asset: *asset,
                        value: change,
                        payer: None,
                    });
                }
            } else {
                // Not enough positive postings. Overdraft accounts cover the
                // shortfall with a negative posting (an offset position); the
                // floor is enforced later in validation. Any other policy fails.
                let policy = self.store.get_account(account).await?.policy;
                match policy {
                    AccountPolicy::CappedOverdraft { .. } | AccountPolicy::UncappedOverdraft => {
                        let positives: Vec<PostingId> = available
                            .iter()
                            .filter(|p| p.value.is_positive())
                            .map(|p| p.id)
                            .collect();
                        consumes.extend_from_slice(&positives);
                        let shortfall = net_debit.checked_sub(total_positive)?;
                        creates.push(NewPosting {
                            owner: *account,
                            asset: *asset,
                            value: shortfall.checked_neg()?,
                            payer: None,
                        });
                    }
                    _ => {
                        return Err(LedgerError::Selection(SelectionError::InsufficientFunds {
                            available: total_positive,
                            requested: *net_debit,
                        }));
                    }
                }
            }
        }

        let mut envelope = EnvelopeBuilder::new()
            .consumes(consumes)
            .creates(creates)
            .book(transfer.book)
            .user_data(transfer.user_data.clone())
            .metadata(transfer.metadata.clone())
            .build();

        // Resolve account snapshots for optimistic concurrency
        let ids = envelope.referenced_accounts();
        envelope.set_account_snapshots(self.resolve_snapshots(&ids).await?);

        Ok(envelope)
    }

    // -----------------------------------------------------------------------
    // Commit: every commit is the envelope saga (reserve -> validate -> finalize)
    // -----------------------------------------------------------------------

    /// Commit a [`Transfer`] intent. Resolves it into a concrete envelope, then
    /// drives the envelope saga. Resolution is read-only, so a crash before the
    /// saga's write-ahead record leaves no partial state.
    #[instrument(skip(self, transfer), fields(book = transfer.book.0), name = "ledger.commit")]
    pub async fn commit(self: &Arc<Self>, transfer: Transfer) -> Result<Receipt, LedgerError> {
        let envelope = self.resolve(&transfer).await?;
        self.commit_envelope(envelope).await
    }

    /// Commit a pre-resolved [`Envelope`] through the saga pipeline (reserve ->
    /// validate -> finalize). This is the single commit path; `commit()` and
    /// `reverse()` both funnel through it.
    ///
    /// Before running, the saga (envelope + reservation) is persisted as a
    /// pending record so a crash mid-commit is completed by [`recover`]. The
    /// record is deleted once the saga reaches a terminal state. The commit is
    /// idempotent on the content-addressed transfer id.
    #[instrument(skip(self, envelope), name = "ledger.commit_envelope")]
    pub async fn commit_envelope(
        self: &Arc<Self>,
        mut envelope: Envelope,
    ) -> Result<Receipt, LedgerError> {
        if envelope.account_snapshots().is_empty() {
            let mut ids: Vec<AccountId> = envelope.creates().iter().map(|p| p.owner).collect();
            ids.sort();
            ids.dedup();
            envelope.set_account_snapshots(self.resolve_snapshots(&ids).await?);
        }

        // Idempotency: an already-committed transfer returns its receipt.
        let tid = envelope_id(&envelope);
        if let Some(record) = self.store.get_transfer(&tid).await? {
            return Ok(record.receipt);
        }

        // Write-ahead: persist {envelope, reservation} so recovery can re-drive.
        let reservation = kuatia_core::ReservationId::default();
        let saga_id = reservation.0;
        let blob = serde_json::to_vec(&PendingSaga {
            envelope: envelope.clone(),
            reservation,
        })
        .map_err(|e| LedgerError::Store(StoreError::Internal(e.to_string())))?;
        self.store.save_saga(&saga_id, blob).await?;

        let result = self.drive_envelope_saga(envelope, reservation).await;

        // Terminal: drop the pending record whether we committed or compensated.
        self.store.delete_saga(&saga_id).await?;
        result
    }

    /// Build and run the envelope saga to a terminal outcome, returning the
    /// resulting receipt.
    async fn drive_envelope_saga(
        self: &Arc<Self>,
        envelope: Envelope,
        reservation: kuatia_core::ReservationId,
    ) -> Result<Receipt, LedgerError> {
        let saga = EnvelopeSaga::new(EnvelopeSagaInputs {
            reserve: ReserveInput,
            validate: ValidateInput,
            finalize: FinalizeInput,
        });
        let ctx = LedgerCtx::for_envelope(Arc::clone(self), envelope, reservation);
        let execution = saga.build(ctx);

        match execution.start().await {
            ExecutionResult::Completed(e) => {
                let ctx = e.into_context();
                ctx.receipts.last().cloned().ok_or_else(|| {
                    LedgerError::Store(StoreError::Internal(
                        "saga completed but no receipt".into(),
                    ))
                })
            }
            ExecutionResult::Failed(_, err) => {
                Err(LedgerError::Store(StoreError::Internal(err.message)))
            }
            ExecutionResult::CompensationFailed {
                original_error,
                compensation_error,
                ..
            } => Err(LedgerError::CompensationFailed {
                original: Box::new(LedgerError::Store(StoreError::Internal(
                    original_error.message,
                ))),
                compensation: Box::new(LedgerError::Store(StoreError::Internal(
                    compensation_error.message,
                ))),
            }),
            ExecutionResult::Paused(_) => Err(LedgerError::Store(StoreError::Internal(
                "saga paused unexpectedly".into(),
            ))),
        }
    }

    /// Re-drive every pending saga to completion. Call on startup to recover
    /// commits interrupted by a crash, returning how many were processed.
    ///
    /// Recovery does **not** re-run reserve/validate — those reject already-
    /// consumed postings, and the envelope was already validated when first
    /// committed. Instead it force-completes the envelope through the idempotent
    /// primitives with the original reservation, so a crash at any point
    /// (pre-reserve, reserved, or mid-finalize) converges to the committed state.
    #[instrument(skip(self), name = "ledger.recover")]
    pub async fn recover(self: &Arc<Self>) -> Result<usize, LedgerError> {
        let pending = self.store.list_pending_sagas().await?;
        let count = pending.len();
        for (saga_id, blob) in pending {
            let PendingSaga {
                envelope,
                reservation,
            } = serde_json::from_slice(&blob)
                .map_err(|e| LedgerError::Store(StoreError::Internal(e.to_string())))?;
            self.complete_envelope(&envelope, reservation).await?;
            self.store.delete_saga(&saga_id).await?;
        }
        Ok(count)
    }

    /// Idempotently push `envelope`'s postings and record to their committed
    /// state. Safe to call from any partial point: each primitive no-ops what is
    /// already done. Used by [`recover`].
    async fn complete_envelope(
        &self,
        envelope: &Envelope,
        reservation: kuatia_core::ReservationId,
    ) -> Result<(), LedgerError> {
        let tid = envelope_id(envelope);
        if self.store.get_transfer(&tid).await?.is_some() {
            return Ok(()); // already committed
        }

        let consumes = envelope.consumes();
        // Reserve then deactivate: this drives Active → PendingInactive → Inactive,
        // and each call no-ops anything already past that state.
        self.store.reserve_postings(consumes, reservation).await?;
        self.store
            .deactivate_postings(consumes, Some(reservation))
            .await?;

        let created: Vec<Posting> = envelope
            .creates()
            .iter()
            .enumerate()
            .map(|(i, np)| {
                Posting::new(
                    PostingId {
                        transfer: tid,
                        index: i as u16,
                    },
                    np.owner,
                    np.asset,
                    np.value,
                )
            })
            .collect();
        self.store.insert_postings(&created).await?;

        let mut involved: Vec<AccountId> = created.iter().map(|p| p.owner).collect();
        if !consumes.is_empty() {
            let consumed = self.store.get_postings(consumes).await?;
            involved.extend(consumed.iter().map(|p| p.owner));
        }
        involved.sort();
        involved.dedup();

        self.store
            .store_transfer(
                EnvelopeRecord {
                    envelope: envelope.clone(),
                    receipt: Receipt { transfer_id: tid },
                    created_at: now_millis()?,
                },
                &involved,
            )
            .await?;
        self.store
            .append_event(&LedgerEvent {
                seq: 0,
                timestamp: now_millis()?,
                kind: LedgerEventKind::TransferCommitted { transfer_id: tid },
            })
            .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Reverse
    // -----------------------------------------------------------------------

    /// Create and commit a reversal envelope for the given envelope id.
    #[instrument(skip(self), name = "ledger.reverse")]
    pub async fn reverse(self: &Arc<Self>, id: &EnvelopeId) -> Result<Receipt, LedgerError> {
        let record = self
            .store
            .get_transfer(id)
            .await?
            .ok_or(LedgerError::TransferNotFound(*id))?;

        let original = &record.envelope;

        let created_posting_ids: Vec<PostingId> = original
            .creates()
            .iter()
            .enumerate()
            .map(|(i, _)| PostingId {
                transfer: record.receipt.transfer_id,
                index: i as u16,
            })
            .collect();

        let original_consumed = if original.consumes().is_empty() {
            vec![]
        } else {
            self.store.get_postings(original.consumes()).await?
        };

        let new_postings: Vec<NewPosting> = original_consumed
            .iter()
            .map(|p| NewPosting {
                owner: p.owner,
                asset: p.asset,
                value: p.value,
                payer: None,
            })
            .collect();

        let reverse_envelope = EnvelopeBuilder::new()
            .consumes(created_posting_ids)
            .creates(new_postings)
            .book(original.book())
            .metadata(original.metadata().clone())
            .build();

        self.commit_envelope(reverse_envelope).await
    }

    // -----------------------------------------------------------------------
    // Internal: resolve account snapshots
    // -----------------------------------------------------------------------

    /// Compute balance from non-Inactive postings for an account/asset pair.
    async fn compute_balance(
        &self,
        account: &AccountId,
        asset: &AssetId,
    ) -> Result<Cent, LedgerError> {
        let postings = self
            .store
            .get_postings_by_account(account, Some(asset), None)
            .await?;
        Ok(Cent::checked_sum(
            postings
                .iter()
                .filter(|p| p.status != PostingStatus::Inactive)
                .map(|p| p.value),
        )?)
    }

    async fn resolve_snapshots(
        &self,
        ids: &[AccountId],
    ) -> Result<Vec<AccountSnapshotId>, LedgerError> {
        let accounts = self.store.get_accounts(ids).await?;
        Ok(accounts.iter().map(account_snapshot_id).collect())
    }

    // -----------------------------------------------------------------------
    // Account lifecycle
    // -----------------------------------------------------------------------

    /// Freeze an account, preventing all transfers.
    #[instrument(skip(self), name = "ledger.freeze")]
    pub async fn freeze(&self, id: &AccountId) -> Result<(), LedgerError> {
        let current = self
            .store
            .get_account(id)
            .await
            .map_err(|_| LedgerError::AccountNotFound(*id))?;
        if current.is_closed() {
            return Err(LedgerError::AccountAlreadyClosed(*id));
        }
        let mut next = current.clone();
        next.version = next.version.checked_add(1).ok_or(LedgerError::Overflow)?;
        next.flags |= kuatia_core::AccountFlags::FROZEN;
        self.store.append_account_version(next).await?;
        self.store
            .append_event(&LedgerEvent {
                seq: 0,
                timestamp: now_millis()?,
                kind: LedgerEventKind::AccountFrozen { account_id: *id },
            })
            .await?;
        Ok(())
    }

    /// Unfreeze a previously frozen account.
    #[instrument(skip(self), name = "ledger.unfreeze")]
    pub async fn unfreeze(&self, id: &AccountId) -> Result<(), LedgerError> {
        let current = self
            .store
            .get_account(id)
            .await
            .map_err(|_| LedgerError::AccountNotFound(*id))?;
        if current.is_closed() {
            return Err(LedgerError::AccountAlreadyClosed(*id));
        }
        let mut next = current.clone();
        next.version = next.version.checked_add(1).ok_or(LedgerError::Overflow)?;
        next.flags.remove(kuatia_core::AccountFlags::FROZEN);
        self.store.append_account_version(next).await?;
        self.store
            .append_event(&LedgerEvent {
                seq: 0,
                timestamp: now_millis()?,
                kind: LedgerEventKind::AccountUnfrozen { account_id: *id },
            })
            .await?;
        Ok(())
    }

    /// Close an account. Must have no active postings.
    #[instrument(skip(self), name = "ledger.close")]
    pub async fn close(&self, id: &AccountId) -> Result<(), LedgerError> {
        let current = self
            .store
            .get_account(id)
            .await
            .map_err(|_| LedgerError::AccountNotFound(*id))?;
        if current.is_closed() {
            return Err(LedgerError::AccountAlreadyClosed(*id));
        }
        // Reject if any posting is still live — Active or PendingInactive
        // (reserved, i.e. a transfer in flight). Only fully Inactive postings
        // (or none) permit a close.
        let blocking = self
            .store
            .get_postings_by_account(id, None, None)
            .await?
            .into_iter()
            .any(|p| p.status != PostingStatus::Inactive);
        if blocking {
            return Err(LedgerError::AccountNotEmpty(*id));
        }
        let mut next = current.clone();
        next.version = next.version.checked_add(1).ok_or(LedgerError::Overflow)?;
        next.flags |= kuatia_core::AccountFlags::CLOSED;
        next.flags.remove(kuatia_core::AccountFlags::FROZEN);
        self.store.append_account_version(next).await?;
        self.store
            .append_event(&LedgerEvent {
                seq: 0,
                timestamp: now_millis()?,
                kind: LedgerEventKind::AccountClosed { account_id: *id },
            })
            .await?;
        Ok(())
    }

    /// Query the current balance of an account for a given asset.
    #[instrument(skip(self), name = "ledger.balance")]
    pub async fn balance(&self, account: &AccountId, asset: &AssetId) -> Result<Cent, LedgerError> {
        self.compute_balance(account, asset).await
    }

    // -----------------------------------------------------------------------
    // Query layer
    // -----------------------------------------------------------------------

    /// List all accounts (latest version of each).
    pub async fn list_accounts(&self) -> Result<Vec<kuatia_core::Account>, LedgerError> {
        Ok(self.store.list_accounts().await?)
    }

    /// Fetch a single account by id.
    pub async fn get_account(&self, id: &AccountId) -> Result<kuatia_core::Account, LedgerError> {
        self.store
            .get_account(id)
            .await
            .map_err(|_| LedgerError::AccountNotFound(*id))
    }

    /// Return all transfers involving the given account.
    pub async fn history(
        &self,
        account: &AccountId,
    ) -> Result<Vec<crate::store::EnvelopeRecord>, LedgerError> {
        Ok(self.store.get_transfers_for_account(account).await?)
    }

    /// Query transfers with filtering and pagination.
    pub async fn query_transfers(
        &self,
        query: &crate::store::TransferQuery,
    ) -> Result<crate::store::Page<crate::store::EnvelopeRecord>, LedgerError> {
        Ok(self.store.query_transfers(query).await?)
    }

    /// Return all postings (any status) for the given account.
    pub async fn postings(
        &self,
        account: &AccountId,
    ) -> Result<Vec<kuatia_core::Posting>, LedgerError> {
        Ok(self
            .store
            .get_postings_by_account(account, None, None)
            .await?)
    }

    /// Query postings with filtering and pagination.
    pub async fn query_postings(
        &self,
        query: &crate::store::PostingQuery,
    ) -> Result<crate::store::Page<kuatia_core::Posting>, LedgerError> {
        Ok(self.store.query_postings(query).await?)
    }

    /// Return the full version history for an account.
    pub async fn account_history(
        &self,
        id: &AccountId,
    ) -> Result<Vec<kuatia_core::Account>, LedgerError> {
        Ok(self.store.get_account_history(id).await?)
    }

    /// Create a new account and emit an AccountCreated event.
    pub async fn create_account(&self, account: kuatia_core::Account) -> Result<(), LedgerError> {
        let id = account.id;
        self.store.create_account(account).await?;
        self.store
            .append_event(&LedgerEvent {
                seq: 0,
                timestamp: now_millis()?,
                kind: LedgerEventKind::AccountCreated { account_id: id },
            })
            .await?;
        Ok(())
    }

    /// Create a new book.
    pub async fn create_book(
        &self,
        book: kuatia_core::Book,
    ) -> Result<(), LedgerError> {
        Ok(self.store.create_book(book).await?)
    }

    /// Fetch a book by id.
    pub async fn get_book(
        &self,
        id: &kuatia_core::BookId,
    ) -> Result<kuatia_core::Book, LedgerError> {
        Ok(self.store.get_book(id).await?)
    }

    /// List all books.
    pub async fn list_books(&self) -> Result<Vec<kuatia_core::Book>, LedgerError> {
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

/// State loaded in phase 1, passed to the pure validation in phase 2.
pub struct LoadedState {
    /// Postings being consumed by the envelope.
    pub consumed_postings: Vec<Posting>,
    /// Accounts referenced by the envelope.
    pub accounts: HashMap<AccountId, kuatia_core::Account>,
    /// Current balances for all referenced (account, asset) pairs.
    pub balances: HashMap<(AccountId, AssetId), Cent>,
    /// The book gating this transfer, if one is loaded (`None` = unrestricted default).
    pub book: Option<Book>,
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use kuatia_core::{Account, AccountFlags, TransferBuilder, UserData};
    use kuatia_storage::mem_store::InMemoryStore;
    use std::collections::BTreeMap;

    fn acct(id: i64, policy: AccountPolicy) -> Account {
        Account {
            id: AccountId::new(id),
            version: 1,
            policy,
            flags: AccountFlags::empty(),
            book: kuatia_core::BookId(0),
            user_data: UserData::default(),
            metadata: BTreeMap::new(),
        }
    }

    /// A commit interrupted right after its write-ahead record (before any step)
    /// is completed by `recover()`: the postings move and the record is cleared.
    #[tokio::test]
    async fn recover_redrives_pending_saga() {
        let ledger = Arc::new(Ledger::new(InMemoryStore::new()));
        for (id, p) in [
            (1, AccountPolicy::NoOverdraft),
            (2, AccountPolicy::NoOverdraft),
            (99, AccountPolicy::ExternalAccount),
        ] {
            ledger.store().create_account(acct(id, p)).await.unwrap();
        }
        // Fund account 1.
        let deposit = TransferBuilder::new()
            .deposit(AccountId::new(1), AssetId::new(1), Cent::from(100), AccountId::new(99))
            .unwrap()
            .build();
        ledger.commit(deposit).await.unwrap();

        // Resolve a pay envelope but persist it as a pending saga WITHOUT running
        // it — simulating a crash right after the write-ahead record.
        let pay = TransferBuilder::new()
            .pay(AccountId::new(1), AccountId::new(2), AssetId::new(1), Cent::from(40))
            .build();
        let envelope = ledger.resolve(&pay).await.unwrap();
        let reservation = kuatia_core::ReservationId::default();
        let blob = serde_json::to_vec(&PendingSaga {
            envelope,
            reservation,
        })
        .unwrap();
        ledger.store().save_saga(&reservation.0, blob).await.unwrap();

        // Recover re-drives it to completion.
        assert_eq!(ledger.recover().await.unwrap(), 1);
        assert_eq!(
            ledger.balance(&AccountId::new(2), &AssetId::new(1)).await.unwrap(),
            Cent::from(40)
        );
        assert_eq!(
            ledger.balance(&AccountId::new(1), &AssetId::new(1)).await.unwrap(),
            Cent::from(60)
        );
        assert!(ledger.store().list_pending_sagas().await.unwrap().is_empty());
    }

    /// A commit that crashed **mid-finalize** — consumed posting already flipped
    /// to Inactive but the transfer record not yet written — is still completed by
    /// `recover()` (reserve/validate are skipped; the primitives no-op the done work).
    #[tokio::test]
    async fn recover_completes_partial_finalize() {
        let ledger = Arc::new(Ledger::new(InMemoryStore::new()));
        for (id, p) in [
            (1, AccountPolicy::NoOverdraft),
            (2, AccountPolicy::NoOverdraft),
            (99, AccountPolicy::ExternalAccount),
        ] {
            ledger.store().create_account(acct(id, p)).await.unwrap();
        }
        let deposit = TransferBuilder::new()
            .deposit(AccountId::new(1), AssetId::new(1), Cent::from(100), AccountId::new(99))
            .unwrap()
            .build();
        ledger.commit(deposit).await.unwrap();

        // Resolve a pay envelope and manually run the commit halfway: reserve the
        // consumed posting and deactivate it (now Inactive), then "crash" — the
        // transfer record and created postings were never written.
        let pay = TransferBuilder::new()
            .pay(AccountId::new(1), AccountId::new(2), AssetId::new(1), Cent::from(40))
            .build();
        let envelope = ledger.resolve(&pay).await.unwrap();
        let reservation = kuatia_core::ReservationId::default();
        let consumes = envelope.consumes().to_vec();
        ledger.store().reserve_postings(&consumes, reservation).await.unwrap();
        let n = ledger
            .store()
            .deactivate_postings(&consumes, Some(reservation))
            .await
            .unwrap();
        assert_eq!(n, 1); // consumed posting is now Inactive

        let blob = serde_json::to_vec(&PendingSaga {
            envelope,
            reservation,
        })
        .unwrap();
        ledger.store().save_saga(&reservation.0, blob).await.unwrap();

        // Recovery finishes the commit despite reserve/validate being unable to
        // re-run over the already-consumed posting.
        assert_eq!(ledger.recover().await.unwrap(), 1);
        assert_eq!(
            ledger.balance(&AccountId::new(2), &AssetId::new(1)).await.unwrap(),
            Cent::from(40)
        );
        assert_eq!(
            ledger.balance(&AccountId::new(1), &AssetId::new(1)).await.unwrap(),
            Cent::from(60)
        );
        assert!(ledger.store().list_pending_sagas().await.unwrap().is_empty());
    }
}
