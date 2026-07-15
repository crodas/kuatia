//! Saga helpers for the ledger.
//!
//! Two things live here:
//!
//! # Dumb-storage count contract
//!
//! `verify_postings`, `verify_posting_states`, and `ensure` interpret the
//! affected-row counts the dumb store returns (ADR-0003): a full count is
//! success, a short count is tolerated only when the postings already reached
//! the intended end-state, and any other mismatch is a genuine fault. The
//! single-commit pipeline (`reserve → finalize`) is a plain function in
//! [`crate::ledger`], not a `legend` saga, and uses these helpers directly.
//!
//! # High-level composition
//!
//! [`PayMovementStep`] and [`DepositMovementStep`] are [`Step`] implementations
//! over the intent-layer API (each wraps `commit()`), so several transfers can
//! be combined into one multi-transfer saga via `legend!`, with automatic LIFO
//! compensation (reversal) across the workflow.

use std::sync::Arc;

use async_trait::async_trait;
use legend::step::{CompensationOutcome, Step, StepOutcome};
use serde::{Deserialize, Serialize};

use kuatia_core::{AccountId, AssetId, Cent, PostingId, PostingState, Receipt, TransferBuilder};

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

/// Fail with an internal fault unless `condition` holds. Flattens the recurring
/// "re-read after a dumb write and assert the end-state, else it's a genuine
/// storage fault" check into one line at each finalize step.
pub(crate) fn ensure(condition: bool, what: impl Into<String>) -> Result<(), LedgerError> {
    if condition {
        Ok(())
    } else {
        Err(internal(what))
    }
}

/// Re-read the derived states of `ids` and confirm every one satisfies `ok`
/// (and that all are present). This is the end-state half of the dumb-storage
/// count contract: the store applied some rows and returned a count; here the
/// saga confirms the postings actually reached the state it intended, treating
/// any mismatch as a genuine fault (contended or concurrently modified).
pub(crate) async fn verify_posting_states(
    store: &dyn Store,
    ids: &[PostingId],
    ok: impl Fn(&PostingState) -> bool,
    what: &str,
) -> Result<(), LedgerError> {
    let states = store
        .get_posting_states(ids)
        .await
        .map_err(LedgerError::Store)?;
    ensure(
        states.len() == ids.len() && states.iter().all(&ok),
        format!(
            "{what}: posting end-state not satisfied ({}/{} rows)",
            states.len(),
            ids.len()
        ),
    )
}

/// Interpret a dumb primitive's affected-row `count` against the `ids` it
/// targeted. `count == ids.len()` is success. A short count is acceptable only if
/// the shortfall is already in the desired end-state — a prior attempt (or this
/// saga, replayed by recovery) already applied it — verified by
/// [`verify_posting_states`]. Otherwise it is a genuine failure (contended or
/// concurrently modified) and the caller compensates.
pub(crate) async fn verify_postings(
    store: &dyn Store,
    ids: &[PostingId],
    count: u64,
    ok: impl Fn(&PostingState) -> bool,
    what: &str,
) -> Result<(), LedgerError> {
    if count == ids.len() as u64 {
        return Ok(());
    }
    verify_posting_states(store, ids, ok, what).await
}

// ---------------------------------------------------------------------------
// Saga context -- carries the ledger handle + state between steps
// ---------------------------------------------------------------------------

/// Saga context that wraps a ledger and collects the receipts a multi-transfer
/// saga produces across its steps.
///
/// The ledger handle is `#[serde(skip)]` -- after deserializing a paused
/// execution you must call [`inject_ledger`](LedgerCtx::inject_ledger)
/// before resuming.
#[derive(Clone, Serialize, Deserialize)]
pub struct LedgerCtx {
    /// Receipts collected from completed steps.
    pub receipts: Vec<Receipt>,
    #[serde(skip)]
    ledger: Option<Arc<Ledger>>,
}

impl std::fmt::Debug for LedgerCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LedgerCtx")
            .field("receipts", &self.receipts)
            .field("ledger_present", &self.ledger.is_some())
            .finish()
    }
}

impl LedgerCtx {
    /// Create a new context wrapping the given ledger.
    pub fn new(ledger: Arc<Ledger>) -> Self {
        Self {
            receipts: Vec::new(),
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
