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
    FinalizeInput, FinalizeTransferStep, LedgerCtx, ReserveInput, ReservePostingsStep,
    ResolveInput, ResolveStep, SagaError, ValidateInput, ValidateTransferStep,
};
use kuatia_storage::error::StoreError;
use kuatia_storage::events::{LedgerEvent, LedgerEventKind};
use kuatia_storage::store::{CommitRequest, EnvelopeRecord, Store};

#[allow(missing_docs)]
mod transfer_saga {
    use super::*;
    legend! {
        TransferSaga<LedgerCtx, SagaError> {
            resolve: ResolveStep,
            reserve: ReservePostingsStep,
            validate: ValidateTransferStep,
            finalize: FinalizeTransferStep,
        }
    }
}
use transfer_saga::*;

/// Async ledger resource composing the four-phase commit pipeline.
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

    /// Phase 3: apply the plan to storage and return a receipt.
    #[instrument(skip(self, envelope, plan), name = "ledger.apply")]
    pub async fn apply(
        &self,
        envelope: &Envelope,
        plan: &kuatia_core::Plan,
    ) -> Result<Receipt, LedgerError> {
        let receipt = Receipt {
            transfer_id: plan.transfer_id,
        };

        // Raw path: consumed postings are Active (never reserved), so pass
        // `reservation: None`. Postings, transfer record, account index, and
        // event commit atomically.
        let events = [LedgerEvent {
            seq: 0,
            timestamp: now_millis()?,
            kind: LedgerEventKind::TransferCommitted {
                transfer_id: receipt.transfer_id,
            },
        }];

        self.store
            .commit_transfer(CommitRequest {
                deactivate: &plan.postings_to_deactivate,
                create: &plan.postings_to_create,
                cas_guards: &plan.cas_guards,
                account_guards: &plan.account_guards,
                reservation: None,
                record: EnvelopeRecord {
                    envelope: envelope.clone(),
                    receipt: receipt.clone(),
                    created_at: now_millis()?,
                },
                events: &events,
            })
            .await?;

        Ok(receipt)
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
    // Atomic commit (raw path -- used by reverse() and internal callers)
    // -----------------------------------------------------------------------

    /// Load, validate, and apply an envelope in one shot (no saga).
    #[instrument(skip(self, envelope), name = "ledger.commit_atomic")]
    pub async fn commit_atomic(&self, mut envelope: Envelope) -> Result<Receipt, LedgerError> {
        if envelope.account_snapshots().is_empty() {
            let mut ids: Vec<AccountId> = envelope.creates().iter().map(|p| p.owner).collect();
            ids.sort();
            ids.dedup();
            envelope.set_account_snapshots(self.resolve_snapshots(&ids).await?);
        }

        let tid = envelope_id(&envelope);
        if let Some(record) = self.store.get_transfer(&tid).await? {
            return Ok(record.receipt);
        }

        let loaded = self.load(&envelope).await?;
        let plan = self.plan(&envelope, &loaded)?;
        self.apply(&envelope, &plan).await
    }

    // -----------------------------------------------------------------------
    // Saga commit: resolve -> reserve -> validate -> finalize (via legend)
    // -----------------------------------------------------------------------

    /// Commit a transfer intent using the saga pipeline driven by legend.
    ///
    /// Steps: resolve movements into envelope -> reserve consumed postings ->
    /// validate -> finalize.
    /// On failure, legend compensates completed steps in reverse order.
    #[instrument(skip(self, transfer), fields(book = transfer.book.0), name = "ledger.commit")]
    pub async fn commit(self: &Arc<Self>, transfer: Transfer) -> Result<Receipt, LedgerError> {
        let saga = TransferSaga::new(TransferSagaInputs {
            resolve: ResolveInput {
                transfer: transfer.clone(),
            },
            reserve: ReserveInput,
            validate: ValidateInput,
            finalize: FinalizeInput,
        });

        let ctx = LedgerCtx::new(Arc::clone(self));
        let execution = saga.build(ctx);

        match execution.start().await {
            ExecutionResult::Completed(e) => {
                let ctx = e.into_context();
                ctx.receipts.last().cloned().ok_or_else(|| {
                    LedgerError::Store(kuatia_storage::error::StoreError::Internal(
                        "saga completed but no receipt".into(),
                    ))
                })
            }
            ExecutionResult::Failed(_, err) => Err(LedgerError::Store(
                kuatia_storage::error::StoreError::Internal(err.message),
            )),
            ExecutionResult::CompensationFailed {
                original_error,
                compensation_error,
                ..
            } => Err(LedgerError::CompensationFailed {
                original: Box::new(LedgerError::Store(
                    kuatia_storage::error::StoreError::Internal(original_error.message),
                )),
                compensation: Box::new(LedgerError::Store(
                    kuatia_storage::error::StoreError::Internal(compensation_error.message),
                )),
            }),
            ExecutionResult::Paused(_) => Err(LedgerError::Store(
                kuatia_storage::error::StoreError::Internal("saga paused unexpectedly".into()),
            )),
        }
    }

    // -----------------------------------------------------------------------
    // Reverse
    // -----------------------------------------------------------------------

    /// Create and commit a reversal envelope for the given envelope id.
    #[instrument(skip(self), name = "ledger.reverse")]
    pub async fn reverse(&self, id: &EnvelopeId) -> Result<Receipt, LedgerError> {
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

        self.commit_atomic(reverse_envelope).await
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
