//! The write-ahead saga/commit engine: resolve, reserve, finalize, recover.
//!
//! This is the deep core of the ledger. Every commit is the two-step envelope
//! saga (`reserve → finalize`, validation inside finalize) with automatic retry
//! and LIFO compensation. A phase-tracked write-ahead record ([`PendingSaga`])
//! lets [`Ledger::recover`] complete or safely abandon a commit interrupted by a
//! crash.

use std::collections::HashMap;
use std::sync::Arc;

use tracing::instrument;

use kuatia_core::{
    AccountId, AccountPolicy, AccountSnapshotId, AssetId, Book, Cent, DEFAULT_BOOK, Envelope,
    EnvelopeBuilder, EnvelopeId, NewPosting, PlanInput, Posting, PostingFilter, PostingId,
    PostingState, Receipt, ResolveInput, Transfer, account_snapshot_id, draft_movements,
    envelope_id, resolve_envelope, validate_and_plan,
};

use kuatia_storage::error::StoreError;
use kuatia_storage::events::{LedgerEvent, LedgerEventKind};
use kuatia_storage::store::EnvelopeRecord;

use super::{Ledger, now_millis};
use crate::error::LedgerError;

/// Phase of an in-flight commit, persisted with the write-ahead record so
/// recovery knows whether validation has completed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
enum SagaPhase {
    /// Saved before reserve. Validation has not necessarily run, so recovery must
    /// re-reserve and re-validate before it can commit.
    Reserving,
    /// Saved at the start of finalize — after validation passed and just before
    /// the consumed postings begin being removed from the reserved index (the
    /// point of no return). Recovery rolls forward without re-validating.
    Finalizing,
}

/// Write-ahead record for an in-flight commit, persisted via `SagaStore` before
/// the saga mutates anything and removed once it reaches a terminal state. On
/// startup [`Ledger::recover`] completes any that survive a crash.
#[derive(serde::Serialize, serde::Deserialize)]
struct PendingSaga {
    envelope: Envelope,
    reservation: kuatia_core::ReservationId,
    phase: SagaPhase,
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

/// Number of times a commit phase is retried before it fails. Matches the
/// former per-step `RetryPolicy::retries(3)`: one initial attempt plus three
/// retries.
const COMMIT_STEP_RETRIES: u8 = 3;

/// Run an idempotent commit phase, retrying up to [`COMMIT_STEP_RETRIES`] times
/// on error. No backoff: the reserve CAS and the idempotent finalize either
/// converge on an immediate retry or fail deterministically.
async fn retry<T, F, Fut>(mut phase: F) -> Result<T, LedgerError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, LedgerError>>,
{
    let mut attempt = 0u8;
    loop {
        match phase().await {
            Ok(value) => return Ok(value),
            Err(error) if attempt >= COMMIT_STEP_RETRIES => return Err(error),
            Err(_) => attempt += 1,
        }
    }
}

impl Ledger {
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
    /// The decision is pure ([`kuatia_core::draft_movements`] +
    /// [`kuatia_core::resolve_envelope`]); this method only loads the state those
    /// functions need. Pass 1 aggregates net debits and tells us which postings
    /// and account policies to load; pass 2 selects postings, computes change,
    /// and covers any overdraft shortfall.
    #[instrument(skip(self, transfer), name = "ledger.resolve")]
    pub async fn resolve(&self, transfer: &Transfer) -> Result<Envelope, LedgerError> {
        let draft = draft_movements(transfer)?;

        // Load the active postings and account policy for each debit. A deposit
        // nets to zero on the system account, so it produces no debit and loads
        // nothing here.
        let mut available: HashMap<(AccountId, AssetId), Vec<Posting>> = HashMap::new();
        let mut policies: HashMap<AccountId, AccountPolicy> = HashMap::new();
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
            if let std::collections::hash_map::Entry::Vacant(slot) = policies.entry(debit.account) {
                slot.insert(self.store.get_account(&debit.account).await?.policy);
            }
        }

        let mut envelope = resolve_envelope(ResolveInput {
            transfer,
            draft,
            available: &available,
            policies: &policies,
        })?;

        // Resolve account snapshots for optimistic concurrency
        let ids = envelope.referenced_accounts();
        envelope.set_account_snapshots(self.resolve_snapshots(&ids).await?);

        Ok(envelope)
    }

    // -----------------------------------------------------------------------
    // Commit: every commit is the envelope saga (reserve -> finalize; finalize re-validates)
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
    /// pending record so a crash mid-commit is completed by [`recover`](Self::recover). The
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

        // Write-ahead: persist {envelope, reservation, phase=Reserving} before any
        // mutation. The finalize step bumps the phase to Finalizing.
        let reservation = kuatia_core::ReservationId::default();
        let saga_id = reservation.0;
        self.save_pending(&envelope, reservation, SagaPhase::Reserving)
            .await?;

        let result = self.drive_envelope_saga(envelope, reservation).await;

        // Delete the pending record only when it is safe: on success, or on a
        // failure that never reached finalize (phase still Reserving → the saga's
        // compensation released our reservation, nothing of ours was applied). If
        // finalize started (Finalizing) and failed, keep it so `recover()` rolls
        // the half-applied commit forward.
        let safe_to_delete = match &result {
            Ok(_) => true,
            Err(_) => self.read_pending_phase(saga_id).await? != Some(SagaPhase::Finalizing),
        };
        if safe_to_delete {
            self.store.delete_saga(&saga_id).await?;
        }
        result
    }

    /// Drive the two-phase envelope commit (`reserve → finalize`) to a terminal
    /// outcome, returning the resulting receipt.
    ///
    /// This is the single-commit pipeline. It is a plain function, not a `legend`
    /// saga: the durability engine is the phase-tracked write-ahead record plus
    /// `recover()` (ADR-0003), and multi-transfer composition happens one level
    /// up, where `PayMovementStep`/`DepositMovementStep` wrap `commit()`. The two
    /// phases and their failure handling mirror the former saga exactly:
    ///
    /// - **reserve** (retried): CAS the consumed postings into the reserved
    ///   index and verify the end-state. A failure after retries leaves nothing
    ///   to roll back here — recovery re-runs the still-`Reserving` saga.
    /// - **finalize** (retried): re-validate, mark `Finalizing`, then run the
    ///   dumb primitives. A failure after retries compensates the reserve phase
    ///   by releasing the reservation (a no-op once the postings are `Spent`,
    ///   i.e. past the point of no return, where recovery rolls forward). If the
    ///   release itself fails, the two errors surface as `CompensationFailed`.
    async fn drive_envelope_saga(
        self: &Arc<Self>,
        envelope: Envelope,
        reservation: kuatia_core::ReservationId,
    ) -> Result<Receipt, LedgerError> {
        let consumes = envelope.consumes().to_vec();

        retry(|| self.reserve_and_verify(&consumes, reservation)).await?;

        match retry(|| self.finalize_envelope(&envelope, reservation)).await {
            Ok(receipt) => Ok(receipt),
            Err(finalize_error) => {
                match self.store.release_postings(&consumes, reservation).await {
                    Ok(_) => Err(finalize_error),
                    Err(release_error) => Err(LedgerError::CompensationFailed {
                        original: Box::new(finalize_error),
                        compensation: Box::new(LedgerError::Store(release_error)),
                    }),
                }
            }
        }
    }

    /// Reserve `consumes` for this saga: CAS each consumed posting from the
    /// active index into the reserved index under `reservation`, then confirm the
    /// end-state (all reserved by us) via the dumb-storage count contract. A short
    /// count is tolerated only when the shortfall is already reserved by us (an
    /// idempotent replay); anything else is a genuine failure.
    async fn reserve_and_verify(
        &self,
        consumes: &[PostingId],
        reservation: kuatia_core::ReservationId,
    ) -> Result<(), LedgerError> {
        let reserved = self
            .store
            .reserve_postings(consumes, reservation)
            .await
            .map_err(LedgerError::Store)?;
        crate::saga::verify_postings(
            self.store.as_ref(),
            consumes,
            reserved,
            |s| matches!(s, PostingState::Reserved(r) if *r == reservation),
            "reserve",
        )
        .await
    }

    /// Complete every pending saga left by a crash. Call on startup; returns how
    /// many were processed.
    ///
    /// Recovery branches on the persisted phase. A `Reserving` saga had not
    /// necessarily validated, so it is re-run through the real saga (which
    /// re-reserves and **re-validates** — aborting cleanly if the postings were
    /// taken or an account was frozen meanwhile). A `Finalizing` saga had already
    /// validated and owns its postings, so it is rolled forward through the
    /// verified `finalize_envelope`. Either way the record is removed only once
    /// the work is committed or safely abandoned.
    #[instrument(skip(self), name = "ledger.recover")]
    pub async fn recover(self: &Arc<Self>) -> Result<usize, LedgerError> {
        let pending = self.store.list_pending_sagas().await?;
        let count = pending.len();
        for (saga_id, blob) in pending {
            let PendingSaga {
                envelope,
                reservation,
                phase,
            } = serde_json::from_slice(&blob)
                .map_err(|e| LedgerError::Store(StoreError::Internal(e.to_string())))?;

            // The transfer record is durable, but a full commit is more than the
            // transfer row: it also includes the committed event, appended *after*
            // store_transfer. A crash in that window leaves the record present yet
            // the event missing, so repair the whole end-state (idempotent) before
            // clearing the pending record.
            let tid = envelope_id(&envelope);
            if self.store.get_transfer(&tid).await?.is_some() {
                self.append_committed_event(tid).await?;
                self.store.delete_saga(&saga_id).await?;
                continue;
            }

            match phase {
                SagaPhase::Finalizing => {
                    // Validation passed and the postings are ours; roll forward.
                    // Keep the record if completion fails so a later run retries.
                    if self.finalize_envelope(&envelope, reservation).await.is_ok() {
                        self.store.delete_saga(&saga_id).await?;
                    }
                }
                SagaPhase::Reserving => {
                    // Re-run the validating saga. On failure, delete only if it did
                    // not reach finalize (clean abort); otherwise keep for next run.
                    let result = self.drive_envelope_saga(envelope, reservation).await;
                    let safe_to_delete = result.is_ok()
                        || self.read_pending_phase(saga_id).await? != Some(SagaPhase::Finalizing);
                    if safe_to_delete {
                        self.store.delete_saga(&saga_id).await?;
                    }
                }
            }
        }
        Ok(count)
    }

    /// Idempotently finalize `envelope` to its committed state, **verifying every
    /// step's end-state**. Used by the saga's finalize step and by recovery.
    ///
    /// When the consumed postings are still reserved it re-validates against
    /// current state (the last-step floor / freeze-close guard) and then marks
    /// the saga `Finalizing` (the point of no return). Once any consumed posting
    /// is already spent — a prior attempt or recovery passed that point — it
    /// rolls forward without re-validating. It never creates or stores anything
    /// unless **all** consumed postings are confirmed spent, which is the
    /// double-spend guard.
    pub(crate) async fn finalize_envelope(
        &self,
        envelope: &Envelope,
        reservation: kuatia_core::ReservationId,
    ) -> Result<Receipt, LedgerError> {
        let tid = envelope_id(envelope);
        if let Some(record) = self.store.get_transfer(&tid).await? {
            // The transfer record is durable, but a crash (or a retried finalize)
            // can land between store_transfer and the event append below. The
            // committed end-state includes the event, so ensure it before
            // returning — `append_committed_event` is idempotent.
            self.append_committed_event(tid).await?;
            return Ok(record.receipt); // already committed
        }
        let consumes = envelope.consumes();

        // Read consumed postings (immutable rows, kept for owner indexing) and
        // their derived states.
        let consumed = if consumes.is_empty() {
            Vec::new()
        } else {
            self.store.get_postings(consumes).await?
        };
        let states = if consumes.is_empty() {
            Vec::new()
        } else {
            self.store.get_posting_states(consumes).await?
        };
        let past_no_return = states.contains(&PostingState::Spent);

        // Last-step boundary re-check: re-validate floor + freeze/close + snapshots
        // against current state, but only while it is still safe (validation
        // rejects a consumed posting that is no longer live).
        if !past_no_return {
            let loaded = self.load(envelope).await?;
            self.plan(envelope, &loaded)?;
        }

        // Point of no return: record Finalizing before any posting is consumed.
        self.save_pending(envelope, reservation, SagaPhase::Finalizing)
            .await?;

        // Consume our reserved postings (remove from the reserved index → spent),
        // then assert ALL consumed postings are spent. This is the double-spend
        // guard: `deactivate_postings(Some(rid))` only removes rows we reserved,
        // so any consumed id still active or reserved by another saga leaves the
        // "all spent" check failing.
        self.store
            .deactivate_postings(consumes, Some(reservation))
            .await?;
        if !consumes.is_empty() {
            crate::saga::verify_posting_states(
                self.store.as_ref(),
                consumes,
                |s| *s == PostingState::Spent,
                "finalize: consumed postings not all spent (contended or not reserved by this saga)",
            )
            .await?;
        }

        // Created postings, derived deterministically from the envelope.
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
        if !created.is_empty() {
            let ids: Vec<PostingId> = created.iter().map(|p| p.id).collect();
            crate::saga::ensure(
                self.store.get_postings(&ids).await?.len() == created.len(),
                "finalize: created postings missing after insert",
            )?;
        }

        // Index both created and consumed owners.
        let mut involved: Vec<AccountId> = created.iter().map(|p| p.owner).collect();
        involved.extend(consumed.iter().map(|p| p.owner));
        involved.sort();
        involved.dedup();

        let receipt = Receipt { transfer_id: tid };
        self.store
            .store_transfer(
                EnvelopeRecord {
                    envelope: envelope.clone(),
                    receipt: receipt.clone(),
                    created_at: now_millis()?,
                },
                &involved,
            )
            .await?;
        crate::saga::ensure(
            self.store.get_transfer(&tid).await?.is_some(),
            "finalize: transfer record missing after store",
        )?;

        self.append_committed_event(tid).await?;
        Ok(receipt)
    }

    /// Idempotently append the `TransferCommitted` event for `tid`.
    ///
    /// The event append is the final finalize step, *after* `store_transfer`, so a
    /// crash in that window leaves a stored transfer with no event. Recovery and a
    /// retried finalize both call this to repair the committed end-state.
    /// `append_event` dedups on the transfer id, so calling it more than once for
    /// the same transfer is a no-op.
    async fn append_committed_event(&self, tid: EnvelopeId) -> Result<(), LedgerError> {
        self.store
            .append_event(&LedgerEvent {
                seq: 0,
                timestamp: now_millis()?,
                kind: LedgerEventKind::TransferCommitted { transfer_id: tid },
            })
            .await?;
        Ok(())
    }

    /// Persist the write-ahead pending-saga record (upsert on the reservation id).
    async fn save_pending(
        &self,
        envelope: &Envelope,
        reservation: kuatia_core::ReservationId,
        phase: SagaPhase,
    ) -> Result<(), LedgerError> {
        let blob = serde_json::to_vec(&PendingSaga {
            envelope: envelope.clone(),
            reservation,
            phase,
        })
        .map_err(|e| LedgerError::Store(StoreError::Internal(e.to_string())))?;
        self.store.save_saga(&reservation.0, blob).await?;
        Ok(())
    }

    /// Read the persisted phase of a pending saga, if it still exists.
    async fn read_pending_phase(&self, saga_id: i64) -> Result<Option<SagaPhase>, LedgerError> {
        for (id, blob) in self.store.list_pending_sagas().await? {
            if id == saga_id {
                let pending: PendingSaga = serde_json::from_slice(&blob)
                    .map_err(|e| LedgerError::Store(StoreError::Internal(e.to_string())))?;
                return Ok(Some(pending.phase));
            }
        }
        Ok(None)
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

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use kuatia_core::{Account, AccountFlags, ReservationId, TransferBuilder};
    use kuatia_storage::mem_store::InMemoryStore;
    use std::collections::BTreeMap;

    fn acct(id: i64, policy: AccountPolicy) -> Account {
        Account {
            id: AccountId::new(id),
            version: 1,
            policy,
            flags: AccountFlags::empty(),
            book: kuatia_core::BookId(0),
            metadata: BTreeMap::new(),
        }
    }

    async fn funded_ledger() -> Arc<Ledger> {
        let ledger = Arc::new(Ledger::new(InMemoryStore::new()));
        for (id, p) in [
            (1, AccountPolicy::NoOverdraft),
            (2, AccountPolicy::NoOverdraft),
            (3, AccountPolicy::NoOverdraft),
            (99, AccountPolicy::ExternalAccount),
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

    async fn save_pending(
        ledger: &Arc<Ledger>,
        envelope: &Envelope,
        rid: ReservationId,
        phase: SagaPhase,
    ) {
        let blob = serde_json::to_vec(&PendingSaga {
            envelope: envelope.clone(),
            reservation: rid,
            phase,
        })
        .unwrap();
        ledger.store().save_saga(&rid.0, blob).await.unwrap();
    }

    /// A commit interrupted right after its write-ahead record (phase Reserving,
    /// before any step) is re-run and completed by `recover()`.
    #[tokio::test]
    async fn recover_redrives_reserving_saga() {
        let ledger = funded_ledger().await;
        let envelope = ledger.resolve(&pay_transfer()).await.unwrap();
        let rid = ReservationId::default();
        save_pending(&ledger, &envelope, rid, SagaPhase::Reserving).await;

        assert_eq!(ledger.recover().await.unwrap(), 1);
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
        assert!(
            ledger
                .store()
                .list_pending_sagas()
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A commit that crashed mid-finalize (phase Finalizing; the consumed posting
    /// is already spent) is rolled forward by `recover()`.
    #[tokio::test]
    async fn recover_completes_partial_finalize() {
        let ledger = funded_ledger().await;
        let envelope = ledger.resolve(&pay_transfer()).await.unwrap();
        let rid = ReservationId::default();
        // Run the commit halfway: reserve + deactivate the consumed posting.
        let consumes = envelope.consumes().to_vec();
        ledger
            .store()
            .reserve_postings(&consumes, rid)
            .await
            .unwrap();
        assert_eq!(
            ledger
                .store()
                .deactivate_postings(&consumes, Some(rid))
                .await
                .unwrap(),
            1
        );
        save_pending(&ledger, &envelope, rid, SagaPhase::Finalizing).await;

        assert_eq!(ledger.recover().await.unwrap(), 1);
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
        assert!(
            ledger
                .store()
                .list_pending_sagas()
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A commit that crashed *after* `store_transfer` but *before* the committed
    /// event was appended (phase Finalizing, transfer row present, event missing)
    /// is repaired by `recover()`: the full end-state includes the event, so
    /// recovery appends it (idempotently) instead of treating the transfer row as
    /// proof of a complete commit.
    #[tokio::test]
    async fn recover_appends_missing_committed_event() {
        let ledger = funded_ledger().await;
        let envelope = ledger.resolve(&pay_transfer()).await.unwrap();
        let tid = envelope_id(&envelope);
        let rid = ReservationId::default();

        // Replay finalize by hand up to and including store_transfer, stopping
        // short of the event append — exactly the crash window.
        let consumes = envelope.consumes().to_vec();
        ledger
            .store()
            .reserve_postings(&consumes, rid)
            .await
            .unwrap();
        ledger
            .store()
            .deactivate_postings(&consumes, Some(rid))
            .await
            .unwrap();
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
        ledger.store().insert_postings(&created).await.unwrap();
        let consumed = ledger.store().get_postings(&consumes).await.unwrap();
        let mut involved: Vec<AccountId> = created.iter().map(|p| p.owner).collect();
        involved.extend(consumed.iter().map(|p| p.owner));
        involved.sort();
        involved.dedup();
        ledger
            .store()
            .store_transfer(
                EnvelopeRecord {
                    envelope: envelope.clone(),
                    receipt: Receipt { transfer_id: tid },
                    created_at: 0,
                },
                &involved,
            )
            .await
            .unwrap();
        save_pending(&ledger, &envelope, rid, SagaPhase::Finalizing).await;

        // Precondition: the transfer is stored, but no committed event exists yet.
        let committed = |evs: &[LedgerEvent]| {
            evs.iter().any(|e| {
                matches!(
                    e.kind,
                    LedgerEventKind::TransferCommitted { transfer_id } if transfer_id == tid
                )
            })
        };
        assert!(ledger.store().get_transfer(&tid).await.unwrap().is_some());
        assert!(!committed(&ledger.get_events_since(0, 1000).await.unwrap()));

        assert_eq!(ledger.recover().await.unwrap(), 1);

        // The missing event is repaired and the pending record cleared.
        assert!(committed(&ledger.get_events_since(0, 1000).await.unwrap()));
        assert!(
            ledger
                .store()
                .list_pending_sagas()
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Recovery of a `Reserving` saga re-validates against current state: if an
    /// account was frozen after the write-ahead record, the commit is abandoned —
    /// no postings move, the reservation is released, and the record is cleared.
    #[tokio::test]
    async fn recover_revalidates_and_aborts_when_account_frozen() {
        let ledger = funded_ledger().await;
        let envelope = ledger.resolve(&pay_transfer()).await.unwrap();
        let tid = envelope_id(&envelope);
        let rid = ReservationId::default();
        save_pending(&ledger, &envelope, rid, SagaPhase::Reserving).await;

        // A freeze lands before recovery runs.
        ledger.freeze(&AccountId::new(1)).await.unwrap();

        assert_eq!(ledger.recover().await.unwrap(), 1);
        // Nothing committed; balances unchanged; reservation released.
        assert!(ledger.store().get_transfer(&tid).await.unwrap().is_none());
        assert_eq!(
            ledger
                .balance(&AccountId::new(1), &AssetId::new(1))
                .await
                .unwrap(),
            Cent::from(100)
        );
        assert_eq!(
            ledger
                .balance(&AccountId::new(2), &AssetId::new(1))
                .await
                .unwrap(),
            Cent::ZERO
        );
        let active = ledger
            .store()
            .get_postings_by_account(1, None, Some(&AssetId::new(1)), PostingFilter::Active)
            .await
            .unwrap();
        assert_eq!(active.len(), 1); // back to Active
        assert!(
            ledger
                .store()
                .list_pending_sagas()
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Recovery cannot double-spend: if the consumed posting was taken by another
    /// transfer while the saga was pending, recovery aborts without creating or
    /// storing anything.
    #[tokio::test]
    async fn recover_does_not_double_spend_a_taken_posting() {
        let ledger = funded_ledger().await;
        let envelope = ledger.resolve(&pay_transfer()).await.unwrap();
        let tid = envelope_id(&envelope);
        let rid = ReservationId::default();
        save_pending(&ledger, &envelope, rid, SagaPhase::Reserving).await;

        // Another transfer consumes account 1's posting and commits.
        let steal = TransferBuilder::new()
            .pay(
                AccountId::new(1),
                AccountId::new(3),
                AssetId::new(1),
                Cent::from(50),
            )
            .build();
        ledger.commit(steal).await.unwrap();

        assert_eq!(ledger.recover().await.unwrap(), 1);
        // Our envelope never committed; only the stealing transfer applied.
        assert!(ledger.store().get_transfer(&tid).await.unwrap().is_none());
        assert_eq!(
            ledger
                .balance(&AccountId::new(1), &AssetId::new(1))
                .await
                .unwrap(),
            Cent::from(50)
        );
        assert_eq!(
            ledger
                .balance(&AccountId::new(3), &AssetId::new(1))
                .await
                .unwrap(),
            Cent::from(50)
        );
        assert_eq!(
            ledger
                .balance(&AccountId::new(2), &AssetId::new(1))
                .await
                .unwrap(),
            Cent::ZERO
        );
        assert!(
            ledger
                .store()
                .list_pending_sagas()
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use std::cell::Cell;

    #[tokio::test]
    async fn retry_gives_up_after_max_attempts() {
        let calls = Cell::new(0u8);
        let result: Result<(), LedgerError> = retry(|| {
            calls.set(calls.get() + 1);
            async { Err(LedgerError::Overflow) }
        })
        .await;
        assert!(result.is_err());
        // One initial attempt plus COMMIT_STEP_RETRIES retries.
        assert_eq!(calls.get(), COMMIT_STEP_RETRIES + 1);
    }

    #[tokio::test]
    async fn retry_returns_the_first_success() {
        let calls = Cell::new(0u8);
        let result: Result<u8, LedgerError> = retry(|| {
            let n = calls.get() + 1;
            calls.set(n);
            async move {
                if n >= 2 {
                    Ok(n)
                } else {
                    Err(LedgerError::Overflow)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 2);
        assert_eq!(calls.get(), 2);
    }
}
