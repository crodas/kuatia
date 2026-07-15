//! Legend saga step adapters for the ledger.
//!
//! Provides [`Step`] implementations so the ledger can participate
//! in multi-resource saga workflows, with automatic LIFO compensation across
//! resource boundaries.
//!
//! # Envelope pipeline saga
//!
//! A commit is two saga steps over a pre-resolved [`Envelope`] (resolution runs
//! before the saga, in `Ledger::commit`):
//!
//! 1. **ReservePostingsStep** -- `reserve_postings`: move each consumed posting from the active index into the reserved index under the saga's `ReservationId`; interprets the count via `verify_postings`.
//! 2. **FinalizeTransferStep** -- delegates to `Ledger::finalize_envelope`, which re-validates against current state (the last-step floor / freeze-close guard), marks the saga `Finalizing`, then runs the dumb primitives (`deactivate_postings` → `insert_postings` → `store_transfer` → `append_event`) verifying every end-state.
//!
//! The `EnvelopeSaga` is defined via `legend!` in `ledger.rs` and driven by
//! `commit_envelope()`. Crash recovery (`Ledger::recover`) re-completes a
//! persisted saga using its persisted phase: a `Reserving` saga is re-run
//! (re-validating); a `Finalizing` saga is rolled forward through the same
//! verified `finalize_envelope`.
//!
//! # High-level composition
//!
//! High-level steps (`PayMovementStep` and `DepositMovementStep`) compose over
//! the intent-layer API and can be combined into multi-transfer sagas via `legend!`.

use std::sync::Arc;

use async_trait::async_trait;
use legend::step::{CompensationOutcome, RetryPolicy, Step, StepOutcome};
use serde::{Deserialize, Serialize};
use tracing::Instrument;

use kuatia_core::{
    AccountId, AssetId, Cent, Envelope, PostingId, PostingState, Receipt, ReservationId,
    TransferBuilder,
};

use crate::error::LedgerError;
use crate::ledger::Ledger;
use kuatia_storage::error::StoreError;
use kuatia_storage::store::Store;

/// A saga-internal plumbing fault (missing context, a short row-count that the
/// end-state does not explain). These are genuine internal invariants, distinct
/// from the typed domain errors ([`LedgerError::Validation`], overdraft, frozen)
/// that flow through unchanged, so they map to [`StoreError::Internal`].
fn internal(message: impl Into<String>) -> LedgerError {
    LedgerError::Store(StoreError::Internal(message.into()))
}

/// The single home of the ADR-0003 affected-row count contract, used after every
/// dumb write primitive in the commit path.
///
/// Interpret a primitive's affected-row `count` against the number of rows it
/// `target`ed. `count == target` is success. A short count is acceptable only if
/// the desired end-state already holds (a prior attempt, or this saga replayed by
/// recovery, already applied it), which `verify` re-reads and reports as a bool.
/// Otherwise it is a genuine failure (contended or concurrently modified) and the
/// caller compensates.
pub(crate) async fn apply_and_verify<F, Fut>(
    count: u64,
    target: usize,
    what: &str,
    verify: F,
) -> Result<(), LedgerError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<bool, LedgerError>>,
{
    if count == target as u64 {
        return Ok(());
    }
    if verify().await? {
        return Ok(());
    }
    Err(internal(format!(
        "{what}: storage applied {count}/{target} rows and the end-state is not satisfied"
    )))
}

/// Apply the count contract to a posting primitive whose end-state is a property
/// of the targeted postings: a short count is idempotent-safe only when every
/// targeted posting already satisfies `ok`.
pub(crate) async fn verify_postings(
    store: &dyn Store,
    ids: &[PostingId],
    count: u64,
    ok: impl Fn(&PostingState) -> bool,
    what: &str,
) -> Result<(), LedgerError> {
    apply_and_verify(count, ids.len(), what, || async {
        let states = store
            .get_posting_states(ids)
            .await
            .map_err(LedgerError::Store)?;
        Ok(states.len() == ids.len() && states.iter().all(&ok))
    })
    .await
}

// ---------------------------------------------------------------------------
// Saga context -- carries the ledger handle + state between steps
// ---------------------------------------------------------------------------

/// Saga context that wraps a ledger and tracks state across steps.
///
/// The ledger handle is `#[serde(skip)]` -- after deserializing a paused
/// execution you must call [`inject_ledger`](LedgerCtx::inject_ledger)
/// before resuming.
#[derive(Clone, Serialize, Deserialize)]
pub struct LedgerCtx {
    /// Receipts collected from completed steps.
    pub receipts: Vec<Receipt>,
    /// Posting ids reserved so far (for compensation).
    pub reserved_postings: Vec<PostingId>,
    /// Resolved envelope produced by the resolve step.
    pub envelope: Option<Envelope>,
    /// Reservation owner token for this saga's reserved postings. Serialized so
    /// it survives pause/recovery, keeping ownership stable across restarts.
    pub reservation: ReservationId,
    #[serde(skip)]
    ledger: Option<Arc<Ledger>>,
}

impl std::fmt::Debug for LedgerCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LedgerCtx")
            .field("receipts", &self.receipts)
            .field("reserved_postings", &self.reserved_postings.len())
            .field("has_envelope", &self.envelope.is_some())
            .field("ledger_present", &self.ledger.is_some())
            .finish()
    }
}

impl LedgerCtx {
    /// Create a new context wrapping the given ledger.
    pub fn new(ledger: Arc<Ledger>) -> Self {
        Self {
            receipts: Vec::new(),
            reserved_postings: Vec::new(),
            envelope: None,
            reservation: ReservationId::default(),
            ledger: Some(ledger),
        }
    }

    /// Create a context for the envelope pipeline (reserve → finalize; finalize re-validates)
    /// with a pre-resolved envelope and an explicit reservation.
    pub fn for_envelope(
        ledger: Arc<Ledger>,
        envelope: Envelope,
        reservation: ReservationId,
    ) -> Self {
        Self {
            receipts: Vec::new(),
            reserved_postings: Vec::new(),
            envelope: Some(envelope),
            reservation,
            ledger: Some(ledger),
        }
    }

    /// Re-inject the ledger handle after deserializing a paused execution.
    pub fn inject_ledger(&mut self, ledger: Arc<Ledger>) {
        self.ledger = Some(ledger);
    }

    /// Borrow the ledger, returning an error if not injected.
    pub fn ledger(&self) -> Result<&Ledger, LedgerError> {
        self.ledger.as_ref().map(|l| l.as_ref()).ok_or_else(|| {
            internal("ledger not injected -- call inject_ledger() after deserializing")
        })
    }

    /// Clone the ledger `Arc`, returning an error if not injected.
    pub fn ledger_arc(&self) -> Result<Arc<Ledger>, LedgerError> {
        self.ledger.clone().ok_or_else(|| {
            internal("ledger not injected -- call inject_ledger() after deserializing")
        })
    }
}

// ===========================================================================
// Envelope pipeline steps (reserve -> finalize; resolve runs before the saga, validate inside finalize)
// ===========================================================================

// ---------------------------------------------------------------------------
// Step 1: ReservePostingsStep
// ---------------------------------------------------------------------------

/// Input for the reserve step (posting ids come from ctx.envelope).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReserveInput;

/// Reserves consumed postings by CAS: move each from the active index to the
/// reserved index (the delete-returns-one picks a single winner).
///
/// Gets the posting ids from the resolved envelope in the context.
/// Compensation releases all reserved postings back to Active.
pub struct ReservePostingsStep;

#[async_trait]
impl Step<LedgerCtx, LedgerError> for ReservePostingsStep {
    type Input = ReserveInput;

    async fn execute(
        ctx: &mut LedgerCtx,
        _input: &ReserveInput,
    ) -> Result<StepOutcome, LedgerError> {
        async {
            let posting_ids: Vec<PostingId> = ctx
                .envelope
                .as_ref()
                .ok_or_else(|| internal("no envelope in context -- resolve step must run first"))?
                .consumes()
                .to_vec();
            let rid = ctx.reservation;
            let ledger = ctx.ledger_arc()?;
            let store = ledger.store();

            let reserved = store
                .reserve_postings(&posting_ids, rid)
                .await
                .map_err(LedgerError::Store)?;
            // Storage reports the count; the saga decides. A short count is fine
            // only if the shortfall is already reserved by us (idempotent replay).
            verify_postings(
                store,
                &posting_ids,
                reserved,
                |s| matches!(s, PostingState::Reserved(r) if *r == rid),
                "reserve",
            )
            .await?;
            ctx.reserved_postings.extend_from_slice(&posting_ids);
            Ok(StepOutcome::Continue)
        }
        .instrument(tracing::info_span!("saga_step", step = "reserve"))
        .await
    }

    async fn compensate(
        ctx: &mut LedgerCtx,
        _input: &ReserveInput,
    ) -> Result<CompensationOutcome, LedgerError> {
        ctx.ledger()?
            .store()
            .release_postings(&ctx.reserved_postings, ctx.reservation)
            .await
            .map_err(LedgerError::Store)?;
        ctx.reserved_postings.clear();
        Ok(CompensationOutcome::Completed)
    }

    fn retry_policy() -> RetryPolicy {
        RetryPolicy::retries(3)
    }
}

// ---------------------------------------------------------------------------
// Step 2: FinalizeTransferStep
// ---------------------------------------------------------------------------

/// Input for the finalize step (envelope comes from ctx).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalizeInput;

/// Re-validates against current state (the last-step floor / freeze-close guard),
/// then drives the verified, idempotent commit via `Ledger::finalize_envelope`.
///
/// Compensation reverses the finalized envelope (only relevant once committed).
pub struct FinalizeTransferStep;

#[async_trait]
impl Step<LedgerCtx, LedgerError> for FinalizeTransferStep {
    type Input = FinalizeInput;

    async fn execute(
        ctx: &mut LedgerCtx,
        _input: &FinalizeInput,
    ) -> Result<StepOutcome, LedgerError> {
        async {
            let envelope = ctx
                .envelope
                .clone()
                .ok_or_else(|| internal("no envelope in context -- resolve step must run first"))?;
            let rid = ctx.reservation;
            let ledger = ctx.ledger_arc()?;

            // All commit work (re-validate, mark Finalizing, deactivate/insert/
            // store/event with end-state verification) lives in `finalize_envelope`
            // so recovery uses exactly the same path. Its typed error (validation,
            // overdraft, frozen) reaches the caller unchanged.
            let receipt = ledger.finalize_envelope(&envelope, rid).await?;

            ctx.receipts.push(receipt);
            ctx.reserved_postings.clear();
            Ok(StepOutcome::Continue)
        }
        .instrument(tracing::info_span!("saga_step", step = "finalize"))
        .await
    }

    async fn compensate(
        ctx: &mut LedgerCtx,
        _input: &FinalizeInput,
    ) -> Result<CompensationOutcome, LedgerError> {
        if let Some(receipt) = ctx.receipts.pop() {
            ctx.ledger_arc()?.reverse(&receipt.transfer_id).await?;
        }
        Ok(CompensationOutcome::Completed)
    }

    fn retry_policy() -> RetryPolicy {
        RetryPolicy::retries(3)
    }
}

// ===========================================================================
// High-level steps (pay / deposit movement steps)
// ===========================================================================

/// Input for the pay movement saga step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PayInput {
    /// Source account.
    pub from: AccountId,
    /// Destination account.
    pub to: AccountId,
    /// Asset to transfer.
    pub asset: AssetId,
    /// Amount to transfer.
    pub amount: Cent,
}

/// Input for the deposit movement saga step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepositInput {
    /// Account receiving the deposit.
    pub to: AccountId,
    /// Asset being deposited.
    pub asset: AssetId,
    /// Amount to deposit.
    pub amount: Cent,
    /// External account funding the deposit.
    pub external: AccountId,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn compensate_last_receipt(ctx: &mut LedgerCtx) -> Result<CompensationOutcome, LedgerError> {
    let receipt = ctx
        .receipts
        .pop()
        .ok_or_else(|| internal("no receipt to compensate"))?;
    ctx.ledger_arc()?.reverse(&receipt.transfer_id).await?;
    Ok(CompensationOutcome::Completed)
}

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

/// Saga step: pay between two accounts via a single-movement transfer.
pub struct PayMovementStep;

#[async_trait]
impl Step<LedgerCtx, LedgerError> for PayMovementStep {
    type Input = PayInput;

    async fn execute(ctx: &mut LedgerCtx, input: &PayInput) -> Result<StepOutcome, LedgerError> {
        let ledger = ctx.ledger_arc()?;
        let transfer = TransferBuilder::new()
            .pay(input.from, input.to, input.asset, input.amount)
            .build();
        let receipt = ledger.commit(transfer).await?;
        ctx.receipts.push(receipt);
        Ok(StepOutcome::Continue)
    }

    async fn compensate(
        ctx: &mut LedgerCtx,
        _input: &PayInput,
    ) -> Result<CompensationOutcome, LedgerError> {
        compensate_last_receipt(ctx).await
    }
}

/// Saga step: deposit value from an external account via a single-movement transfer.
pub struct DepositMovementStep;

#[async_trait]
impl Step<LedgerCtx, LedgerError> for DepositMovementStep {
    type Input = DepositInput;

    async fn execute(
        ctx: &mut LedgerCtx,
        input: &DepositInput,
    ) -> Result<StepOutcome, LedgerError> {
        let ledger = ctx.ledger_arc()?;
        let transfer = TransferBuilder::new()
            .deposit(input.to, input.asset, input.amount, input.external)
            .map_err(LedgerError::from)?
            .build();
        let receipt = ledger.commit(transfer).await?;
        ctx.receipts.push(receipt);
        Ok(StepOutcome::Continue)
    }

    async fn compensate(
        ctx: &mut LedgerCtx,
        _input: &DepositInput,
    ) -> Result<CompensationOutcome, LedgerError> {
        compensate_last_receipt(ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[tokio::test]
    async fn full_count_is_ok_without_re_reading() {
        let verified = Cell::new(false);
        let result = apply_and_verify(3, 3, "reserve", || {
            verified.set(true);
            async { Ok(true) }
        })
        .await;
        assert!(result.is_ok());
        assert!(
            !verified.get(),
            "a full count must not re-read the end-state"
        );
    }

    #[tokio::test]
    async fn short_count_is_ok_when_end_state_already_holds() {
        // Idempotent replay: a prior attempt applied the shortfall.
        let result = apply_and_verify(2, 3, "reserve", || async { Ok(true) }).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn short_count_is_internal_error_when_end_state_missing() {
        let result = apply_and_verify(2, 3, "reserve", || async { Ok(false) }).await;
        assert!(matches!(
            result,
            Err(LedgerError::Store(StoreError::Internal(_)))
        ));
    }

    #[tokio::test]
    async fn verify_error_propagates() {
        let result = apply_and_verify(0, 1, "store", || async {
            Err(LedgerError::Store(StoreError::Internal(
                "read failed".into(),
            )))
        })
        .await;
        assert!(matches!(
            result,
            Err(LedgerError::Store(StoreError::Internal(_)))
        ));
    }
}
