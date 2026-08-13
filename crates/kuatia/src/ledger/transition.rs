//! One account-version transition, shared by freeze / unfreeze / close.
//!
//! Every lifecycle flag change is the same shape: load the account, reject if it
//! is already closed, then append a new version with the flag flipped and the
//! matching lifecycle event. This module holds that shape once, parameterized by
//! the flag mutation and the event.
//!
//! The version append and the event append are one atomic store write
//! ([`commit_transition`](kuatia_storage::store::CommitStore::commit_transition)),
//! so a crash cannot leave a version bump with no event. The store re-checks the
//! version chain and a closed account inside the transaction.

use kuatia_core::{AccountFlags, AccountId};
use kuatia_storage::events::{LedgerEvent, LedgerEventKind};
use kuatia_storage::store::{TransitionOutcome, TransitionRejection};

use super::{Ledger, now_millis};
use crate::error::LedgerError;

impl Ledger {
    /// Append one new account version with `mutate` applied to its flags, then
    /// emit the lifecycle event produced by `make_event` (given the account id and
    /// the new version), atomically. Rejects a closed account.
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
        let event = LedgerEvent {
            seq: 0,
            timestamp: now_millis()?,
            kind: make_event(*id, next.version),
        };

        match self.store.commit_transition(next, event).await? {
            TransitionOutcome::Applied | TransitionOutcome::AlreadyApplied => Ok(()),
            TransitionOutcome::Rejected(TransitionRejection::AlreadyClosed(account)) => {
                Err(LedgerError::AccountAlreadyClosed(account))
            }
            TransitionOutcome::Rejected(TransitionRejection::VersionConflict {
                account,
                expected,
            }) => Err(LedgerError::AccountVersionConflict { account, expected }),
        }
    }
}
