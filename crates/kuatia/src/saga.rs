//! Ledger commit helpers and high-level saga steps.
//!
//! # Count contract
//!
//! `apply_and_verify`, `verify_postings`, and `consume_reserved` encode the
//! ADR-0003 affected-row-count rule applied after every dumb write primitive in
//! the commit path. The commit path itself (reserve → finalize) is a linear
//! method on [`Ledger`] (`ledger::commit`), not a `legend` saga: for a single
//! commit the two steps were pass-throughs, so collapsing them keeps the
//! reserve/compensation policy next to the logic it governs (refines ADR-0002).
//!
//! # High-level composition
//!
//! [`PayMovementStep`] and [`DepositMovementStep`] wrap the intent-layer
//! `Ledger::commit` as `legend` [`Step`]s, so several transfers compose into one
//! multi-transfer saga (an FX trade, a multi-leg settlement) with LIFO
//! compensation across the whole workflow. This is where `legend` earns its keep.

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use legend::step::{CompensationOutcome, Step, StepOutcome};
use serde::{Deserialize, Serialize};

use kuatia_core::{
    AccountId, AssetId, Cent, PostingId, PostingState, Receipt, ReservationId, TransferBuilder,
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
    Fut: Future<Output = Result<bool, LedgerError>>,
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

/// The authoritative double-spend / reservation-ownership guard.
///
/// Consume the reserved postings, then assert every consumed id is now `Spent`.
/// `deactivate_postings(_, Some(rid))` removes *only* rows this saga reserved, so
/// the "all Spent" assertion can only pass when no consumed id was left active or
/// held by another saga: that is what forbids a double-spend.
///
/// This CAS is the real concurrency authority for the consumed-posting lifecycle.
/// The pure lifecycle check in [`validate_and_plan`](kuatia_core::validate_and_plan)
/// is a snapshot-in-time, best-effort read (ADR-0003); this is the check that
/// holds under contention. It runs once the saga is past its point of no return
/// (phase `Finalizing`). See ADR-0021 for the full commit-safety map.
pub(crate) async fn consume_reserved(
    store: &dyn Store,
    consumes: &[PostingId],
    reservation: ReservationId,
) -> Result<(), LedgerError> {
    let spent = store
        .deactivate_postings(consumes, Some(reservation))
        .await
        .map_err(LedgerError::Store)?;
    verify_postings(
        store,
        consumes,
        spent,
        |s| *s == PostingState::Spent,
        "finalize: consume reserved postings",
    )
    .await
}

// ---------------------------------------------------------------------------
// Saga context -- carries the ledger handle + state between steps
// ---------------------------------------------------------------------------

/// Saga context that wraps a ledger and collects the receipts of the transfers a
/// multi-transfer saga commits, for LIFO compensation.
///
/// The ledger handle is `#[serde(skip)]`: it is supplied when the context is
/// constructed and is not part of the serialized form.
#[derive(Clone, Serialize, Deserialize)]
pub struct LedgerCtx {
    /// Receipts collected from completed steps, popped in reverse to compensate.
    pub receipts: Vec<Receipt>,
    #[serde(skip)]
    ledger: Option<Arc<Ledger>>,
}

impl fmt::Debug for LedgerCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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

    /// Borrow the ledger, returning an error if the handle is absent.
    pub fn ledger(&self) -> Result<&Ledger, LedgerError> {
        self.ledger
            .as_ref()
            .map(|l| l.as_ref())
            .ok_or_else(|| internal("ledger handle missing from saga context"))
    }

    /// Clone the ledger `Arc`, returning an error if the handle is absent.
    pub fn ledger_arc(&self) -> Result<Arc<Ledger>, LedgerError> {
        self.ledger
            .clone()
            .ok_or_else(|| internal("ledger handle missing from saga context"))
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
    use kuatia_core::{EnvelopeId, Posting};
    use kuatia_storage::mem_store::InMemoryStore;
    use kuatia_storage::store::PostingStore;
    use std::cell::Cell;

    fn active_posting(store_seed: u8) -> Posting {
        Posting::new(
            PostingId {
                transfer: EnvelopeId([store_seed; 32]),
                index: 0,
            },
            AccountId::new(1),
            AssetId::new(1),
            Cent::from(100),
        )
    }

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

    /// The guard consumes the postings this saga reserved: they end `Spent`.
    #[tokio::test]
    async fn consume_reserved_spends_our_postings() {
        let store = InMemoryStore::new();
        let p = active_posting(1);
        store
            .insert_postings(std::slice::from_ref(&p))
            .await
            .unwrap();
        let rid = ReservationId::default();
        store.reserve_postings(&[p.id], rid).await.unwrap();

        consume_reserved(&store, &[p.id], rid).await.unwrap();

        let states = store.get_posting_states(&[p.id]).await.unwrap();
        assert_eq!(states, vec![PostingState::Spent]);
    }

    /// An unreserved (still active) posting is refused, and left untouched:
    /// `deactivate_postings(_, Some(rid))` removes nothing we do not own.
    #[tokio::test]
    async fn consume_reserved_refuses_unreserved_posting() {
        let store = InMemoryStore::new();
        let p = active_posting(2);
        store
            .insert_postings(std::slice::from_ref(&p))
            .await
            .unwrap();

        let err = consume_reserved(&store, &[p.id], ReservationId::default())
            .await
            .unwrap_err();
        assert!(matches!(err, LedgerError::Store(StoreError::Internal(_))));
        let states = store.get_posting_states(&[p.id]).await.unwrap();
        assert_eq!(states, vec![PostingState::Active]);
    }

    /// The double-spend guard: a posting reserved by another saga is refused, and
    /// stays reserved by that saga. Our deactivate removes nothing, so the
    /// "all Spent" assertion fails.
    #[tokio::test]
    async fn consume_reserved_refuses_posting_held_by_another_saga() {
        let store = InMemoryStore::new();
        let p = active_posting(3);
        store
            .insert_postings(std::slice::from_ref(&p))
            .await
            .unwrap();
        let theirs = ReservationId::default();
        store.reserve_postings(&[p.id], theirs).await.unwrap();

        let err = consume_reserved(&store, &[p.id], ReservationId::default())
            .await
            .unwrap_err();
        assert!(matches!(err, LedgerError::Store(StoreError::Internal(_))));
        let states = store.get_posting_states(&[p.id]).await.unwrap();
        assert_eq!(states, vec![PostingState::Reserved(theirs)]);
    }
}
