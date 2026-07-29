//! The write-ahead record awaiting recovery, and how each kind completes.
//!
//! A commit (reserve → finalize) and an account-version transition (append
//! version → append event) are each more than one store write with no shared
//! transaction, so a crash mid-sequence can leave a half-applied state. Before
//! either mutates anything it persists a [`PendingRecord`] via `SagaStore`; on
//! startup [`Ledger::recover`](super::Ledger::recover) loads every surviving
//! record and drives it to a terminal state through [`PendingRecord::complete`].
//!
//! This module owns the whole write-ahead concept behind one seam: what a
//! pending record *is*, how it is (de)serialized, how it is persisted, and how
//! each kind completes. The completion primitives it calls (`finalize_envelope`,
//! `drive_envelope_saga`) stay on [`Ledger`] because the live commit path shares
//! them; this module sequences them for the recovery path.

use std::sync::Arc;

use kuatia_core::{Account, Envelope, ReservationId, envelope_id};
use kuatia_storage::error::StoreError;
use kuatia_storage::events::{LedgerEvent, LedgerEventKind};

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
#[derive(serde::Serialize, serde::Deserialize)]
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

/// The two kinds of write-ahead record the [`SagaStore`] holds, tagged so
/// recovery can tell an envelope commit saga from an account transition and
/// complete each through its own path.
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) enum PendingRecord {
    /// A two-step envelope commit saga (reserve → finalize).
    Envelope(PendingSaga),
    /// A single account-version transition (append version + lifecycle event).
    Transition(PendingTransition),
}

impl PendingRecord {
    /// A commit write-ahead record at the given phase.
    pub(super) fn envelope(
        envelope: Envelope,
        reservation: ReservationId,
        phase: SagaPhase,
    ) -> Self {
        Self::Envelope(PendingSaga {
            envelope,
            reservation,
            phase,
        })
    }

    /// An account-transition write-ahead record.
    pub(super) fn transition(next: Account, event: LedgerEventKind) -> Self {
        Self::Transition(PendingTransition { next, event })
    }

    /// Decode a record from its stored bytes. The single decoder for the
    /// write-ahead format, shared by `recover` and the keyed phase read.
    pub(super) fn decode(blob: &[u8]) -> Result<Self, LedgerError> {
        serde_json::from_slice(blob)
            .map_err(|e| LedgerError::Store(StoreError::Internal(e.to_string())))
    }

    /// The commit phase of an envelope record; `None` for a transition record,
    /// which has no phase.
    pub(super) fn envelope_phase(&self) -> Option<SagaPhase> {
        match self {
            Self::Envelope(s) => Some(s.phase),
            Self::Transition(_) => None,
        }
    }

    /// Persist this record under `saga_id` (upsert on the id).
    pub(super) async fn save(&self, ledger: &Ledger, saga_id: i64) -> Result<(), LedgerError> {
        let blob = serde_json::to_vec(self)
            .map_err(|e| LedgerError::Store(StoreError::Internal(e.to_string())))?;
        ledger.store.save_saga(&saga_id, blob).await?;
        Ok(())
    }

    /// Drive this record to a terminal state and clear it when safe. Called by
    /// [`Ledger::recover`](super::Ledger::recover) for every surviving record.
    ///
    /// A transition rolls forward (any completion error propagates, so recovery
    /// retries on the next run). An envelope commit branches on its phase, and
    /// its drive/finalize failures are absorbed here (the record is kept for a
    /// later run) rather than aborting recovery of the remaining records.
    pub(super) async fn complete(
        self,
        ledger: &Arc<Ledger>,
        saga_id: i64,
    ) -> Result<(), LedgerError> {
        match self {
            Self::Transition(PendingTransition { next, event }) => {
                complete_transition(ledger, saga_id, next, event).await
            }
            Self::Envelope(PendingSaga {
                envelope,
                reservation,
                phase,
            }) => complete_envelope(ledger, saga_id, envelope, reservation, phase).await,
        }
    }
}

/// Roll a crash-interrupted transition forward and clear its write-ahead record.
///
/// Idempotent in every crash window: the version append runs only into an empty
/// version slot (`append_account_version` requires `version == current + 1`, so a
/// blind retry after it applied would fail), and the event carries its target
/// version so re-appending it dedups to the original. The empty-slot guard also
/// subsumes the forward path's is_closed check: a close always bumps the version,
/// so a since-closed account sits at `version >= next.version` and is skipped.
async fn complete_transition(
    ledger: &Ledger,
    saga_id: i64,
    next: Account,
    event: LedgerEventKind,
) -> Result<(), LedgerError> {
    // The account is guaranteed to exist here (its version was bumped, or is
    // about to be), so a read failure is transient or a real invariant breach,
    // not "not found": surface it verbatim so recovery retries.
    let current = ledger.store.get_account(&next.id).await?;
    if current.version < next.version {
        ledger.store.append_account_version(next).await?;
    }
    ledger
        .store
        .append_event(&LedgerEvent {
            seq: 0,
            timestamp: now_millis()?,
            kind: event,
        })
        .await?;
    ledger.store.delete_saga(&saga_id).await?;
    Ok(())
}

/// Complete a crash-interrupted commit and clear its record when safe.
async fn complete_envelope(
    ledger: &Arc<Ledger>,
    saga_id: i64,
    envelope: Envelope,
    reservation: ReservationId,
    phase: SagaPhase,
) -> Result<(), LedgerError> {
    // The transfer record is durable, but a full commit is more than the transfer
    // row: it also includes the committed event, appended *after* store_transfer.
    // A crash in that window leaves the record present yet the event missing, so
    // repair the whole end-state (idempotent) before clearing the record.
    let tid = envelope_id(&envelope);
    if ledger.store.get_transfer(&tid).await?.is_some() {
        ledger.append_committed_event(tid).await?;
        ledger.store.delete_saga(&saga_id).await?;
        return Ok(());
    }

    match phase {
        SagaPhase::Finalizing => {
            // Validation passed and the postings are ours; roll forward. Keep the
            // record if completion fails so a later run retries.
            if ledger
                .finalize_envelope(&envelope, reservation)
                .await
                .is_ok()
            {
                ledger.store.delete_saga(&saga_id).await?;
            }
        }
        SagaPhase::Reserving => {
            // Re-run the validating saga. On failure, delete only if it did not
            // reach finalize (clean abort); otherwise keep for the next run.
            let result = ledger.drive_envelope_saga(envelope, reservation).await;
            let safe_to_delete =
                result.is_ok() || ledger.saga_phase(saga_id).await? != Some(SagaPhase::Finalizing);
            if safe_to_delete {
                ledger.store.delete_saga(&saga_id).await?;
            }
        }
    }
    Ok(())
}
