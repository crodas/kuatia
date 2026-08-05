//! One account-version transition, shared by freeze / unfreeze / close.
//!
//! Every lifecycle flag change is the same shape: load the account, reject if it
//! is already closed, append a new version with the flag flipped, then append the
//! matching lifecycle event. This module holds that shape once, parameterized by
//! the flag mutation and the event, and routes it through the same write-ahead /
//! repair path the commit engine uses.
//!
//! The durability point: the version append and the event append are two separate
//! store writes with no shared transaction. A crash between them would otherwise
//! leave a version bump with no event and nothing to repair it. Persisting a
//! write-ahead [`PendingTransition`](super::pending) before either write lets
//! [`Ledger::recover`](Ledger) roll the transition forward. Recovery is
//! idempotent both ways: the version append is skipped when the version is
//! already present, and the event carries its target version so a second append
//! dedups to the original.

use kuatia_core::{AccountFlags, AccountId, TransitionId};
use kuatia_storage::events::{LedgerEvent, LedgerEventKind};

use super::pending::PendingTransition;
use super::{Ledger, now_millis};
use crate::error::LedgerError;

impl Ledger {
    /// Append one new account version with `mutate` applied to its flags, then
    /// emit the lifecycle event produced by `make_event` (given the account id and
    /// the new version). Rejects a closed account. A write-ahead record persisted
    /// before the writes lets [`recover`](Ledger::recover) complete a transition
    /// interrupted between the two appends.
    ///
    /// Callers layer any transition-specific guard (e.g. close's emptiness check)
    /// before calling this.
    pub(super) async fn transition(
        &self,
        id: &AccountId,
        mutate: impl FnOnce(&mut AccountFlags),
        make_event: impl FnOnce(AccountId, u64) -> LedgerEventKind,
    ) -> Result<(), LedgerError> {
        let current = self
            .store
            .get_account(id)
            .await
            .map_err(|_| LedgerError::AccountNotFound(*id))?;
        if current.is_closed() {
            return Err(LedgerError::AccountAlreadyClosed(*id));
        }

        let mut next = current;
        next.version = next.version.checked_add(1).ok_or(LedgerError::Overflow)?;
        mutate(&mut next.flags);
        let event = make_event(*id, next.version);

        // Write-ahead before either write. A crash between the version append and
        // the event append is then repaired by recover(), not left dangling. The
        // record is tagged `Transition` in the store, and its id comes from the
        // shared saga-id generator, so it lands in its own typed slot without
        // colliding with an in-flight commit saga's key.
        let saga_id = TransitionId::default().0;
        PendingTransition::new(next.clone(), event.clone())
            .save(self, saga_id)
            .await?;

        let expected = next.version;
        if self.store.append_account_version(next).await? == 0 {
            // A concurrent transition moved the head; the guarded append matched
            // nothing. Surface the conflict; the write-ahead record is repaired
            // (or cleared) by recover() on the next startup.
            return Err(LedgerError::AccountVersionConflict {
                account: *id,
                expected,
            });
        }
        self.store
            .append_event(&LedgerEvent {
                seq: 0,
                timestamp: now_millis()?,
                kind: event,
            })
            .await?;
        self.store.delete_saga(&saga_id).await?;
        Ok(())
    }
}
