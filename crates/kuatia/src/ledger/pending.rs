//! The write-ahead record awaiting recovery, and how each kind completes.
//!
//! A commit (reserve → finalize) and an account-version transition (append
//! version → append event) are each more than one store write with no shared
//! transaction, so a crash mid-sequence can leave a half-applied state. Before
//! either mutates anything it persists a write-ahead record via `SagaStore`,
//! tagged with its [`SagaKind`]; on startup
//! [`Ledger::recover`](super::Ledger::recover) loads every surviving record and,
//! dispatching on that kind, drives it to a terminal state.
//!
//! This module owns the whole write-ahead concept behind one seam: what a
//! pending record *is* ([`PendingSaga`] / [`PendingTransition`]), how it is
//! (de)serialized, how it is persisted, and how each kind completes. The
//! completion primitives it calls (`finalize_envelope`, `reserve_and_finalize`)
//! stay on [`Ledger`] because the live commit path shares them; this module
//! sequences them for the recovery path.

use std::sync::Arc;

use kuatia_core::{Account, Envelope, Receipt, ReservationId, envelope_id};
use kuatia_storage::error::StoreError;
use kuatia_storage::events::{LedgerEvent, LedgerEventKind};
use kuatia_storage::store::SagaKind;

use super::{Ledger, now_millis};
use crate::error::LedgerError;

/// Phase of an in-flight commit, persisted with the write-ahead record so
/// recovery knows whether validation has completed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub(super) enum SagaPhase {
    /// Saved before reserve. Validation has not necessarily run, so recovery must
    /// re-reserve and re-validate before it can commit.
    Reserving,
    /// Saved at the start of finalize — after validation passed and just before
    /// the consumed postings begin being removed from the reserved index (the
    /// point of no return). Recovery rolls forward without re-validating.
    Finalizing,
}

/// Write-ahead record for an in-flight commit (reserve → finalize). Persisted
/// before the saga mutates anything and removed once it reaches a terminal
/// state.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct PendingSaga {
    pub(super) envelope: Envelope,
    pub(super) reservation: ReservationId,
    pub(super) phase: SagaPhase,
}

/// Write-ahead record for an in-flight account-version transition
/// (freeze/unfreeze/close). The transition appends a new account version and then
/// its lifecycle event; a crash between the two leaves a version bump with no
/// event. Persisting this before either write lets recovery roll the transition
/// forward, re-appending the (idempotent) event.
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct PendingTransition {
    /// The next account version to append: version already bumped, flag flipped.
    pub(super) next: Account,
    /// The lifecycle event paired with this version bump. It carries the target
    /// version, so re-appending it on recovery dedups to the original.
    pub(super) event: LedgerEventKind,
}

/// Map a JSON (de)serialization failure to a store error. The write-ahead format
/// is a store implementation detail, so a codec failure surfaces as `Store`.
fn json_err(e: serde_json::Error) -> LedgerError {
    LedgerError::Store(StoreError::Internal(e.to_string()))
}

impl PendingSaga {
    /// A fresh write-ahead record for a new commit, at the pre-mutation phase.
    pub(super) fn new(envelope: Envelope, reservation: ReservationId) -> Self {
        Self {
            envelope,
            reservation,
            phase: SagaPhase::Reserving,
        }
    }

    /// The same record advanced to the point of no return, used for the finalize
    /// bump just before the consumed postings are removed.
    pub(super) fn finalizing(envelope: Envelope, reservation: ReservationId) -> Self {
        Self {
            envelope,
            reservation,
            phase: SagaPhase::Finalizing,
        }
    }

    /// An envelope saga is always keyed by its reservation id, so the live commit
    /// path and recovery agree on where the record lives.
    fn saga_id(&self) -> i64 {
        self.reservation.0
    }

    /// Decode an envelope commit record from its stored bytes.
    pub(super) fn decode(blob: &[u8]) -> Result<Self, LedgerError> {
        serde_json::from_slice(blob).map_err(json_err)
    }

    /// Persist this record at its current phase (upsert). The single writer of a
    /// commit write-ahead record: the Reserving→Finalizing bump is just a persist
    /// of the [`finalizing`](Self::finalizing) variant.
    pub(super) async fn persist(&self, ledger: &Ledger) -> Result<(), LedgerError> {
        let blob = serde_json::to_vec(self).map_err(json_err)?;
        ledger
            .store
            .save_saga(SagaKind::Envelope, &self.saga_id(), blob)
            .await?;
        Ok(())
    }

    /// Run a fresh commit end to end: write-ahead at Reserving, reserve then
    /// finalize, then clear the record when it is safe. The single home of the commit
    /// write-ahead lifecycle; [`commit_envelope`](Ledger::commit_envelope) calls
    /// this and recovery mirrors it.
    pub(super) async fn run(self, ledger: &Arc<Ledger>) -> Result<Receipt, LedgerError> {
        self.persist(ledger).await?;
        // Commit does not touch the balance projection (ADR-0019): cache points
        // are appended lazily on read, once enough credits/debits have accrued.
        let result = ledger
            .reserve_and_finalize(&self.envelope, self.reservation)
            .await;
        self.clear_if_safe(ledger, result.is_ok()).await?;
        result
    }

    /// Delete the write-ahead record unless the saga crossed its point of no
    /// return on a failing run. The ONE place the delete-safety rule lives: safe
    /// on success, or on a failure that never reached `Finalizing` (compensation
    /// released our reservation, nothing of ours applied). A failure that reached
    /// `Finalizing` keeps the record so recovery rolls the commit forward.
    async fn clear_if_safe(
        &self,
        ledger: &Arc<Ledger>,
        succeeded: bool,
    ) -> Result<(), LedgerError> {
        let safe =
            succeeded || ledger.saga_phase(self.saga_id()).await? != Some(SagaPhase::Finalizing);
        if safe {
            ledger.store.delete_saga(&self.saga_id()).await?;
        }
        Ok(())
    }

    /// Complete a crash-interrupted commit from its persisted phase, clearing the
    /// record when safe. The recovery counterpart of [`run`](Self::run): same
    /// lifecycle rules, entered from a decoded record instead of a fresh one.
    pub(super) async fn complete(self, ledger: &Arc<Ledger>) -> Result<(), LedgerError> {
        // A full commit is the transfer row plus its committed event (appended
        // after store_transfer). If the row is present the commit reached the far
        // side; repair the possibly-missing event (idempotent) and clear.
        let tid = envelope_id(&self.envelope);
        if ledger.store.get_transfer(&tid).await?.is_some() {
            ledger.append_committed_event(tid).await?;
            ledger.store.delete_saga(&self.saga_id()).await?;
            return Ok(());
        }

        match self.phase {
            // Validation passed and the postings are ours; roll forward through the
            // verified finalize. Keep the record if it fails so a later run retries.
            SagaPhase::Finalizing => {
                if ledger
                    .finalize_envelope(&self.envelope, self.reservation)
                    .await
                    .is_ok()
                {
                    ledger.store.delete_saga(&self.saga_id()).await?;
                }
                Ok(())
            }
            // Not past the point of no return: re-run the validating saga and clear
            // under the same rule as a live commit. The saga's own failure is
            // absorbed (record kept for the next run); only infra errors propagate.
            SagaPhase::Reserving => {
                let result = ledger
                    .reserve_and_finalize(&self.envelope, self.reservation)
                    .await;
                self.clear_if_safe(ledger, result.is_ok()).await
            }
        }
    }
}

impl PendingTransition {
    /// A transition write-ahead record for a version bump and its lifecycle event.
    pub(super) fn new(next: Account, event: LedgerEventKind) -> Self {
        Self { next, event }
    }

    /// Decode a transition record from its stored bytes.
    pub(super) fn decode(blob: &[u8]) -> Result<Self, LedgerError> {
        serde_json::from_slice(blob).map_err(json_err)
    }

    /// Persist this record under `saga_id` (upsert).
    pub(super) async fn save(&self, ledger: &Ledger, saga_id: i64) -> Result<(), LedgerError> {
        let blob = serde_json::to_vec(self).map_err(json_err)?;
        ledger
            .store
            .save_saga(SagaKind::Transition, &saga_id, blob)
            .await?;
        Ok(())
    }

    /// Roll a crash-interrupted transition forward and clear its write-ahead
    /// record.
    ///
    /// Idempotent in every crash window: the version append runs only into an
    /// empty version slot (`append_account_version` requires `version == current +
    /// 1`, so a blind retry after it applied would fail), and the event carries its
    /// target version so re-appending it dedups to the original. The empty-slot
    /// guard also subsumes the forward path's is_closed check: a close always bumps
    /// the version, so a since-closed account sits at `version >= next.version` and
    /// is skipped.
    pub(super) async fn complete(self, ledger: &Ledger, saga_id: i64) -> Result<(), LedgerError> {
        // The account is guaranteed to exist here (its version was bumped, or is
        // about to be), so a read failure is transient or a real invariant breach,
        // not "not found": surface it verbatim so recovery retries.
        let current = ledger.store.get_account(&self.next.id).await?;
        if current.version < self.next.version {
            ledger.store.append_account_version(self.next).await?;
        }
        ledger
            .store
            .append_event(&LedgerEvent {
                seq: 0,
                timestamp: now_millis()?,
                kind: self.event,
            })
            .await?;
        ledger.store.delete_saga(&saga_id).await?;
        Ok(())
    }
}
