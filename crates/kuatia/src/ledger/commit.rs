//! The commit engine: resolve, validate, apply atomically, reverse.
//!
//! This is the deep core of the ledger. Every commit resolves a [`Transfer`]
//! intent into a concrete [`Envelope`], validates it against loaded state in the
//! pure core, and hands the validated effects to the store's atomic
//! [`commit_envelope`](kuatia_storage::store::CommitStore::commit_envelope). The
//! store applies them in one transaction and re-checks the stateful guards
//! inside it, so there is no half-applied state and nothing to recover after a
//! crash (ADR-0023, superseding the write-ahead saga of ADR-0003).

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;

use tracing::instrument;

use kuatia_core::{
    Account, AccountId, AccountSnapshotId, AssetId, Book, Cent, DEFAULT_BOOK, Envelope,
    EnvelopeBuilder, EnvelopeId, NewPosting, Plan, PlanInput, Posting, PostingFilter, PostingId,
    Receipt, ResolveInput, Transfer, ValidationError, account_snapshot_id, draft_movements,
    envelope_id, required_state, resolve_envelope, validate_and_plan,
};

use kuatia_storage::error::StoreError;
use kuatia_storage::events::{LedgerEvent, LedgerEventKind};
use kuatia_storage::store::{CommitOutcome, CommitRejection, CommitRequest, EnvelopeRecord};

use super::{Ledger, now_millis};
use crate::error::LedgerError;

/// State loaded in phase 1, passed to the pure validation in phase 2.
struct LoadedState {
    /// Postings being consumed by the envelope.
    consumed_postings: Vec<Posting>,
    /// Accounts referenced by the envelope.
    accounts: HashMap<AccountId, Account>,
    /// Current balances for all referenced (account, asset) pairs.
    balances: HashMap<(AccountId, AssetId), Cent>,
    /// The book gating this transfer, if one is loaded (`None` = unrestricted default).
    book: Option<Book>,
}

impl Ledger {
    // -----------------------------------------------------------------------
    // Validation phases: load (read state) -> plan (pure validate)
    // -----------------------------------------------------------------------

    /// Load all state needed for validation.
    #[instrument(skip(self, envelope), name = "ledger.load")]
    async fn load(&self, envelope: &Envelope) -> Result<LoadedState, LedgerError> {
        let consumed_postings = if envelope.consumes().is_empty() {
            vec![]
        } else {
            self.store.get_postings(envelope.consumes()).await?
        };

        // The pure core names exactly what validation will read; iterate that
        // key-set so the loader cannot silently under-fetch (a missing balance
        // key defaults to zero and would flip an overdraft decision).
        let required = required_state(envelope, &consumed_postings);

        let account_list = self.store.get_accounts(&required.accounts).await?;
        let accounts: HashMap<AccountId, _> = account_list.into_iter().map(|a| (a.id, a)).collect();

        let mut balances = HashMap::new();
        for (account_id, asset_id) in &required.balances {
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

    /// Run pure validation over the loaded state and produce a plan.
    fn plan(&self, envelope: &Envelope, loaded: &LoadedState) -> Result<Plan, LedgerError> {
        // The loader must have fetched every balance key validation reads. A gap
        // means `load` under-fetched: validation would read the missing key as
        // zero and could approve an overdraft, silently creating value. This
        // crosses the async-loader / pure-core seam, so guard it in every build,
        // not just debug.
        if let Some((account, asset)) = required_state(envelope, &loaded.consumed_postings)
            .balances
            .into_iter()
            .find(|key| !loaded.balances.contains_key(key))
        {
            return Err(LedgerError::UnderFetchedState { account, asset });
        }

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
    /// The decision is pure ([`kuatia_core::draft_movements`] +
    /// [`kuatia_core::resolve_envelope`]); this method only loads the state those
    /// functions need. Pass 1 aggregates net debits and tells us which postings
    /// and accounts to load; pass 2 selects postings, computes change, and covers
    /// any overdraft shortfall (reading each account's own flag, the same one
    /// validation reads).
    #[instrument(skip(self, transfer), name = "ledger.resolve")]
    pub async fn resolve(&self, transfer: &Transfer) -> Result<Envelope, LedgerError> {
        let draft = draft_movements(transfer)?;

        // Load the active postings for each debit and the debit accounts
        // themselves. Pass 2 reads the overdraft decision off each account's flag,
        // so we hand it the accounts rather than a re-derived set. A deposit nets
        // to zero on the system account, so it produces no debit and loads nothing
        // here.
        let mut available: HashMap<(AccountId, AssetId), Vec<Posting>> = HashMap::new();
        let mut accounts: HashMap<AccountId, Account> = HashMap::new();
        for debit in &draft.debits {
            let postings = self
                .store
                .get_postings_by_account(
                    debit.account.id,
                    Some(debit.account.sub),
                    Some(&debit.asset),
                    PostingFilter::Active,
                )
                .await?;
            available.insert((debit.account, debit.asset), postings);
            if let Entry::Vacant(e) = accounts.entry(debit.account) {
                e.insert(self.store.get_account(&debit.account).await?);
            }
        }

        let mut envelope = resolve_envelope(ResolveInput {
            transfer,
            draft,
            available: &available,
            accounts: &accounts,
        })?;

        // Resolve account snapshots for optimistic concurrency
        let ids = envelope.referenced_accounts();
        envelope.set_account_snapshots(self.resolve_snapshots(&ids).await?);

        Ok(envelope)
    }

    // -----------------------------------------------------------------------
    // Commit: resolve (read-only) then apply atomically
    // -----------------------------------------------------------------------

    /// Commit a [`Transfer`] intent. Resolves it into a concrete envelope, then
    /// validates and applies it atomically. Resolution is read-only, and the
    /// apply is one store transaction, so a crash leaves no partial state.
    #[instrument(skip(self, transfer), fields(book = transfer.book.0), name = "ledger.commit")]
    pub async fn commit(self: &Arc<Self>, transfer: Transfer) -> Result<Receipt, LedgerError> {
        let envelope = self.resolve(&transfer).await?;
        self.commit_envelope(envelope).await
    }

    /// Commit a pre-resolved [`Envelope`]. Validates it against loaded state,
    /// then applies the validated effects through the store's atomic
    /// [`commit_envelope`](kuatia_storage::store::CommitStore::commit_envelope):
    /// one transaction that spends the consumed postings, creates the new ones,
    /// stores the transfer, and appends the committed event, re-checking
    /// double-spend, freeze/close, and the overdraft floor inside it. This is the
    /// single commit path; `commit()` and `reverse()` both funnel through it, and
    /// it is idempotent on the content-addressed transfer id.
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

        // Idempotency pre-check: an already-committed transfer returns its receipt
        // without re-validating. The store also guards this atomically.
        let tid = envelope_id(&envelope);
        if let Some(record) = self.store.get_transfer(&tid).await? {
            return Ok(record.receipt);
        }

        // Pure validation against loaded state (conservation, ownership,
        // snapshots, book policy, plus a best-effort floor/freeze read). The store
        // then re-checks the stateful guards strictly inside the commit
        // transaction.
        let loaded = self.load(&envelope).await?;
        let plan = self.plan(&envelope, &loaded)?;

        // Index both created and consumed owners; this is also the set the store
        // re-checks for freeze/close/floor.
        let mut involved: Vec<AccountId> =
            plan.postings_to_create.iter().map(|p| p.owner).collect();
        involved.extend(loaded.consumed_postings.iter().map(|p| p.owner));
        involved.sort();
        involved.dedup();

        let ts = now_millis()?;
        let receipt = Receipt {
            transfer_id: plan.transfer_id,
        };
        let record = EnvelopeRecord {
            envelope: envelope.clone(),
            receipt: receipt.clone(),
            created_at: ts,
        };
        let event = LedgerEvent {
            seq: 0,
            timestamp: ts,
            kind: LedgerEventKind::TransferCommitted {
                transfer_id: plan.transfer_id,
            },
        };

        let outcome = self
            .store
            .commit_envelope(CommitRequest {
                transfer_id: plan.transfer_id,
                consume: &plan.postings_to_deactivate,
                create: &plan.postings_to_create,
                record,
                involved: &involved,
                event,
            })
            .await?;
        match outcome {
            CommitOutcome::Committed(r) | CommitOutcome::AlreadyCommitted(r) => Ok(r),
            CommitOutcome::Rejected(reason) => Err(map_rejection(reason)),
        }
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

    async fn resolve_snapshots(
        &self,
        ids: &[AccountId],
    ) -> Result<Vec<AccountSnapshotId>, LedgerError> {
        let accounts = self.store.get_accounts(ids).await?;
        Ok(accounts.iter().map(account_snapshot_id).collect())
    }
}

/// Map a store-side [`CommitRejection`] to the typed ledger error callers match
/// on, so a strict in-transaction guard surfaces the same error a snapshot-time
/// validation would.
fn map_rejection(reason: CommitRejection) -> LedgerError {
    match reason {
        CommitRejection::DoubleSpend(id) => LedgerError::DoubleSpend(id),
        CommitRejection::AccountFrozen(id) => {
            LedgerError::Validation(ValidationError::AccountFrozen(id))
        }
        CommitRejection::AccountClosed(id) => {
            LedgerError::Validation(ValidationError::AccountClosed(id))
        }
        CommitRejection::OverdraftExceeded {
            account,
            asset,
            projected,
        } => LedgerError::Validation(ValidationError::OverdraftExceeded {
            account,
            asset,
            projected,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuatia_core::{Account, AccountFlags, TransferBuilder};
    use kuatia_storage::mem_store::InMemoryStore;
    use std::collections::BTreeMap;

    fn acct(id: i64, flags: AccountFlags) -> Account {
        Account {
            id: AccountId::new(id),
            version: 1,
            flags,
            book: kuatia_core::BookId(0),
            metadata: BTreeMap::new(),
        }
    }

    async fn funded_ledger() -> Arc<Ledger> {
        let ledger = Arc::new(Ledger::new(InMemoryStore::new()));
        for (id, p) in [
            (1, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT),
            (2, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT),
            (3, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT),
            (99, AccountFlags::empty()),
        ] {
            ledger.store().create_account(acct(id, p)).await.unwrap();
        }
        let deposit = TransferBuilder::new()
            .deposit(
                AccountId::new(1),
                AssetId::new(1),
                Cent::from(100),
                AccountId::new(99),
            )
            .unwrap()
            .build();
        ledger.commit(deposit).await.unwrap();
        ledger
    }

    fn pay_transfer() -> Transfer {
        TransferBuilder::new()
            .pay(
                AccountId::new(1),
                AccountId::new(2),
                AssetId::new(1),
                Cent::from(40),
            )
            .build()
    }

    /// A commit spends the payer's posting and credits the payee atomically.
    #[tokio::test]
    async fn commit_moves_value() {
        let ledger = funded_ledger().await;
        ledger.commit(pay_transfer()).await.unwrap();
        assert_eq!(
            ledger
                .balance(&AccountId::new(2), &AssetId::new(1))
                .await
                .unwrap(),
            Cent::from(40)
        );
        assert_eq!(
            ledger
                .balance(&AccountId::new(1), &AssetId::new(1))
                .await
                .unwrap(),
            Cent::from(60)
        );
    }

    /// Committing into a frozen payer is rejected atomically: nothing moves.
    #[tokio::test]
    async fn commit_into_frozen_account_is_rejected() {
        let ledger = funded_ledger().await;
        ledger.freeze(&AccountId::new(1)).await.unwrap();
        let err = ledger.commit(pay_transfer()).await.unwrap_err();
        assert!(matches!(
            err,
            LedgerError::Validation(ValidationError::AccountFrozen(_))
        ));
        assert_eq!(
            ledger
                .balance(&AccountId::new(1), &AssetId::new(1))
                .await
                .unwrap(),
            Cent::from(100)
        );
    }

    /// If the loader under-fetches a balance key validation reads, `plan` fails
    /// loudly rather than letting the missing key default to zero and silently
    /// approve an overdraft. This guards the async-loader / pure-core seam in
    /// every build, not just debug.
    #[tokio::test]
    async fn plan_rejects_under_fetched_balance() -> Result<(), LedgerError> {
        let ledger = funded_ledger().await;
        let envelope = ledger.resolve(&pay_transfer()).await?;
        let mut loaded = ledger.load(&envelope).await?;

        // Drop a key the loader correctly fetched; validation still reads it.
        let dropped = *loaded
            .balances
            .keys()
            .next()
            .expect("pay transfer reads at least one balance");
        loaded.balances.remove(&dropped);

        match ledger.plan(&envelope, &loaded) {
            Err(LedgerError::UnderFetchedState { account, asset }) => {
                assert_eq!((account, asset), dropped);
            }
            other => panic!("expected UnderFetchedState, got {other:?}"),
        }
        Ok(())
    }
}
