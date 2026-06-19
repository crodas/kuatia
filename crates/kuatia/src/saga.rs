//! Legend saga step adapters for the ledger.
//!
//! Provides [`Step`](legend::Step) implementations so the ledger can participate
//! in multi-resource saga workflows. Each step commits a transfer in `execute`
//! and reverses it in `compensate`, giving you automatic rollback across
//! resource boundaries.
//!
//! # Transfer pipeline saga
//!
//! The core transfer pipeline is broken into four saga steps:
//!
//! 1. **ResolveStep** -- resolve a `Transfer` intent into an `Envelope`
//! 2. **ReservePostingsStep** -- CAS each consumed posting from Active to PendingInactive
//! 3. **ValidateTransferStep** -- load accounts/balances, run `validate_and_plan()`
//! 4. **FinalizeTransferStep** -- PendingInactive to Inactive, create new postings, store envelope
//!
//! The `TransferSaga` is defined via `legend!` in `ledger.rs` and driven by
//! `commit()`.
//!
//! # High-level composition
//!
//! High-level steps (`PayMovementStep`, `DepositMovementStep`, etc.) compose over
//! the intent-layer API and can be combined into multi-transfer sagas via `legend!`.

use std::sync::Arc;

use async_trait::async_trait;
use legend::step::{CompensationOutcome, RetryPolicy, Step, StepOutcome};
use serde::{Deserialize, Serialize};
use tracing::Instrument;

use kuatia_core::{
    AccountId, AssetId, Cent, Envelope, Plan, PlanInput, PostingId, Receipt, Transfer,
    TransferBuilder, validate_and_plan,
};

use crate::error::LedgerError;
use crate::ledger::{Ledger, now_millis};
use kuatia_storage::events::{LedgerEvent, LedgerEventKind};
use kuatia_storage::store::EnvelopeRecord;

// ---------------------------------------------------------------------------
// Saga error -- serializable + cloneable wrapper
// ---------------------------------------------------------------------------

/// Serializable error wrapper used across saga steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SagaError {
    /// Human-readable error description.
    pub message: String,
}

impl std::fmt::Display for SagaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SagaError {}

impl From<LedgerError> for SagaError {
    fn from(e: LedgerError) -> Self {
        Self {
            message: e.to_string(),
        }
    }
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
    /// Validated plan produced by the validate step.
    pub plan: Option<Plan>,
    /// Resolved envelope produced by the resolve step.
    pub envelope: Option<Envelope>,
    #[serde(skip)]
    ledger: Option<Arc<Ledger>>,
}

impl std::fmt::Debug for LedgerCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LedgerCtx")
            .field("receipts", &self.receipts)
            .field("reserved_postings", &self.reserved_postings.len())
            .field("has_plan", &self.plan.is_some())
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
            plan: None,
            envelope: None,
            ledger: Some(ledger),
        }
    }

    /// Re-inject the ledger handle after deserializing a paused execution.
    pub fn inject_ledger(&mut self, ledger: Arc<Ledger>) {
        self.ledger = Some(ledger);
    }

    /// Borrow the ledger, returning an error if not injected.
    pub fn ledger(&self) -> Result<&Ledger, SagaError> {
        self.ledger.as_ref().map(|l| l.as_ref()).ok_or(SagaError {
            message: "ledger not injected -- call inject_ledger() after deserializing".into(),
        })
    }

    /// Clone the ledger `Arc`, returning an error if not injected.
    pub fn ledger_arc(&self) -> Result<Arc<Ledger>, SagaError> {
        self.ledger.clone().ok_or(SagaError {
            message: "ledger not injected -- call inject_ledger() after deserializing".into(),
        })
    }
}

// ===========================================================================
// Transfer pipeline steps (resolve -> reserve -> validate -> finalize)
// ===========================================================================

// ---------------------------------------------------------------------------
// Step 1: ResolveStep
// ---------------------------------------------------------------------------

/// Input for the resolve step: the transfer intent to resolve.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveInput {
    /// The transfer intent to resolve into a concrete envelope.
    pub transfer: Transfer,
}

/// Resolves a [`Transfer`] intent into a concrete [`Envelope`] by selecting
/// postings for each movement.
///
/// Compensation is a no-op (no side effects).
pub struct ResolveStep;

#[async_trait]
impl Step<LedgerCtx, SagaError> for ResolveStep {
    type Input = ResolveInput;

    async fn execute(ctx: &mut LedgerCtx, input: &ResolveInput) -> Result<StepOutcome, SagaError> {
        async {
            let ledger = ctx.ledger()?;
            let envelope = ledger
                .resolve(&input.transfer)
                .await
                .map_err(SagaError::from)?;
            ctx.envelope = Some(envelope);
            Ok(StepOutcome::Continue)
        }
        .instrument(tracing::info_span!("saga_step", step = "resolve"))
        .await
    }

    async fn compensate(
        _ctx: &mut LedgerCtx,
        _input: &ResolveInput,
    ) -> Result<CompensationOutcome, SagaError> {
        Ok(CompensationOutcome::Completed)
    }
}

// ---------------------------------------------------------------------------
// Step 2: ReservePostingsStep
// ---------------------------------------------------------------------------

/// Input for the reserve step (posting ids come from ctx.envelope).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReserveInput;

/// Reserves consumed postings by CAS: Active to PendingInactive.
///
/// Gets the posting ids from the resolved envelope in the context.
/// Compensation releases all reserved postings back to Active.
pub struct ReservePostingsStep;

#[async_trait]
impl Step<LedgerCtx, SagaError> for ReservePostingsStep {
    type Input = ReserveInput;

    async fn execute(ctx: &mut LedgerCtx, _input: &ReserveInput) -> Result<StepOutcome, SagaError> {
        async {
            let envelope = ctx.envelope.as_ref().ok_or(SagaError {
                message: "no envelope in context -- resolve step must run first".into(),
            })?;
            let posting_ids: Vec<PostingId> = envelope.consumes().to_vec();

            ctx.ledger()?
                .store()
                .reserve_postings(&posting_ids)
                .await
                .map_err(|e| SagaError::from(LedgerError::Store(e)))?;
            ctx.reserved_postings.extend_from_slice(&posting_ids);
            Ok(StepOutcome::Continue)
        }
        .instrument(tracing::info_span!("saga_step", step = "reserve"))
        .await
    }

    async fn compensate(
        ctx: &mut LedgerCtx,
        _input: &ReserveInput,
    ) -> Result<CompensationOutcome, SagaError> {
        ctx.ledger()?
            .store()
            .release_postings(&ctx.reserved_postings)
            .await
            .map_err(|e| SagaError::from(LedgerError::Store(e)))?;
        ctx.reserved_postings.clear();
        Ok(CompensationOutcome::Completed)
    }

    fn retry_policy() -> RetryPolicy {
        RetryPolicy::retries(3)
    }
}

// ---------------------------------------------------------------------------
// Step 3: ValidateTransferStep
// ---------------------------------------------------------------------------

/// Input for the validate step (envelope comes from ctx).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidateInput;

/// Loads accounts and balances, then runs `validate_and_plan()`.
///
/// Stores the resulting [`Plan`] in the context for the finalize step.
/// Compensation is a no-op (reads only).
pub struct ValidateTransferStep;

#[async_trait]
impl Step<LedgerCtx, SagaError> for ValidateTransferStep {
    type Input = ValidateInput;

    async fn execute(
        ctx: &mut LedgerCtx,
        _input: &ValidateInput,
    ) -> Result<StepOutcome, SagaError> {
        async {
            let envelope = ctx.envelope.as_ref().ok_or(SagaError {
                message: "no envelope in context -- resolve step must run first".into(),
            })?;

            let ledger = ctx.ledger()?;
            let loaded = ledger.load(envelope).await.map_err(SagaError::from)?;

            let plan_input = PlanInput {
                envelope,
                consumed_postings: &loaded.consumed_postings,
                accounts: &loaded.accounts,
                balances: &loaded.balances,
            };

            let plan =
                validate_and_plan(plan_input).map_err(|e| SagaError::from(LedgerError::from(e)))?;
            ctx.plan = Some(plan);
            Ok(StepOutcome::Continue)
        }
        .instrument(tracing::info_span!("saga_step", step = "validate"))
        .await
    }

    async fn compensate(
        _ctx: &mut LedgerCtx,
        _input: &ValidateInput,
    ) -> Result<CompensationOutcome, SagaError> {
        Ok(CompensationOutcome::Completed)
    }
}

// ---------------------------------------------------------------------------
// Step 4: FinalizeTransferStep
// ---------------------------------------------------------------------------

/// Input for the finalize step (envelope and plan come from ctx).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalizeInput;

/// Finalizes the envelope: PendingInactive to Inactive, creates new postings,
/// stores the envelope record.
///
/// Compensation reverses the finalized envelope.
pub struct FinalizeTransferStep;

#[async_trait]
impl Step<LedgerCtx, SagaError> for FinalizeTransferStep {
    type Input = FinalizeInput;

    async fn execute(
        ctx: &mut LedgerCtx,
        _input: &FinalizeInput,
    ) -> Result<StepOutcome, SagaError> {
        async {
            let plan = ctx.plan.take().ok_or(SagaError {
                message: "no plan in context -- validate step must run first".into(),
            })?;

            let envelope = ctx.envelope.as_ref().ok_or(SagaError {
                message: "no envelope in context -- resolve step must run first".into(),
            })?;

            let store = ctx.ledger()?.store();

            store
                .finalize_postings(&plan.postings_to_deactivate, &plan.postings_to_create)
                .await
                .map_err(|e| SagaError::from(LedgerError::Store(e)))?;

            let receipt = Receipt {
                transfer_id: plan.transfer_id,
            };
            store
                .store_transfer(EnvelopeRecord {
                    envelope: envelope.clone(),
                    receipt: receipt.clone(),
                    created_at: now_millis().map_err(SagaError::from)?,
                })
                .await
                .map_err(|e| SagaError::from(LedgerError::Store(e)))?;

            let _ = store
                .append_event(&LedgerEvent {
                    seq: 0,
                    timestamp: now_millis().map_err(SagaError::from)?,
                    kind: LedgerEventKind::TransferCommitted {
                        transfer_id: receipt.transfer_id,
                    },
                })
                .await;

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
    ) -> Result<CompensationOutcome, SagaError> {
        if let Some(receipt) = ctx.receipts.pop() {
            ctx.ledger()?.reverse(&receipt.transfer_id).await?;
        }
        Ok(CompensationOutcome::Completed)
    }

    fn retry_policy() -> RetryPolicy {
        RetryPolicy::retries(3)
    }
}

// ===========================================================================
// High-level steps (pay / deposit / withdraw movement steps)
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

/// Input for the withdraw movement saga step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WithdrawInput {
    /// Account to withdraw from.
    pub from: AccountId,
    /// Asset being withdrawn.
    pub asset: AssetId,
    /// Amount to withdraw.
    pub amount: Cent,
    /// External account receiving the withdrawal.
    pub external: AccountId,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn compensate_last_receipt(ctx: &mut LedgerCtx) -> Result<CompensationOutcome, SagaError> {
    let receipt = ctx.receipts.pop().ok_or(SagaError {
        message: "no receipt to compensate".into(),
    })?;
    ctx.ledger()?.reverse(&receipt.transfer_id).await?;
    Ok(CompensationOutcome::Completed)
}

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

/// Saga step: pay between two accounts via a single-movement transfer.
pub struct PayMovementStep;

#[async_trait]
impl Step<LedgerCtx, SagaError> for PayMovementStep {
    type Input = PayInput;

    async fn execute(ctx: &mut LedgerCtx, input: &PayInput) -> Result<StepOutcome, SagaError> {
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
    ) -> Result<CompensationOutcome, SagaError> {
        compensate_last_receipt(ctx).await
    }
}

/// Saga step: deposit value from an external account via a single-movement transfer.
pub struct DepositMovementStep;

#[async_trait]
impl Step<LedgerCtx, SagaError> for DepositMovementStep {
    type Input = DepositInput;

    async fn execute(ctx: &mut LedgerCtx, input: &DepositInput) -> Result<StepOutcome, SagaError> {
        let ledger = ctx.ledger_arc()?;
        let transfer = TransferBuilder::new()
            .deposit(input.to, input.asset, input.amount, input.external)
            .map_err(|e| SagaError::from(LedgerError::from(e)))?
            .build();
        let receipt = ledger.commit(transfer).await?;
        ctx.receipts.push(receipt);
        Ok(StepOutcome::Continue)
    }

    async fn compensate(
        ctx: &mut LedgerCtx,
        _input: &DepositInput,
    ) -> Result<CompensationOutcome, SagaError> {
        compensate_last_receipt(ctx).await
    }
}

/// Saga step: withdraw value to an external account via a single-movement transfer.
pub struct WithdrawMovementStep;

#[async_trait]
impl Step<LedgerCtx, SagaError> for WithdrawMovementStep {
    type Input = WithdrawInput;

    async fn execute(ctx: &mut LedgerCtx, input: &WithdrawInput) -> Result<StepOutcome, SagaError> {
        let ledger = ctx.ledger_arc()?;
        let transfer = TransferBuilder::new()
            .withdraw(input.from, input.asset, input.amount, input.external)
            .build();
        let receipt = ledger.commit(transfer).await?;
        ctx.receipts.push(receipt);
        Ok(StepOutcome::Continue)
    }

    async fn compensate(
        ctx: &mut LedgerCtx,
        _input: &WithdrawInput,
    ) -> Result<CompensationOutcome, SagaError> {
        compensate_last_receipt(ctx).await
    }
}
