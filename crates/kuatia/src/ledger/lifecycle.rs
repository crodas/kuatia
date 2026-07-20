//! Account lifecycle: create, freeze, unfreeze, close.
//!
//! Accounts are append-only and versioned: each mutation appends a new version
//! rather than editing in place. Freeze/close guards are validate-time and
//! best-effort under concurrency (see the dumb-storage ADR).
//!
//! Freeze, unfreeze, and close are the same version-bump-plus-event shape; they
//! delegate it to [`Ledger::transition`](super::transition), which carries the
//! write-ahead / crash-repair path. Each method here supplies only the flag
//! mutation, the lifecycle event, and any transition-specific guard.

use tracing::instrument;

use kuatia_core::{AccountFlags, AccountId, PostingFilter};
use kuatia_storage::events::{LedgerEvent, LedgerEventKind};

use super::{Ledger, now_millis};
use crate::error::LedgerError;

impl Ledger {
    /// Create a new account and emit an AccountCreated event.
    pub async fn create_account(&self, account: kuatia_core::Account) -> Result<(), LedgerError> {
        let id = account.id;
        self.store.create_account(account).await?;
        self.store
            .append_event(&LedgerEvent {
                seq: 0,
                timestamp: now_millis()?,
                kind: LedgerEventKind::AccountCreated { account_id: id },
            })
            .await?;
        Ok(())
    }

    /// Freeze an account, preventing all transfers.
    #[instrument(skip(self), name = "ledger.freeze")]
    pub async fn freeze(&self, id: &AccountId) -> Result<(), LedgerError> {
        self.transition(
            id,
            |flags| *flags |= AccountFlags::FROZEN,
            |account_id, version| LedgerEventKind::AccountFrozen {
                account_id,
                version,
            },
        )
        .await
    }

    /// Unfreeze a previously frozen account.
    #[instrument(skip(self), name = "ledger.unfreeze")]
    pub async fn unfreeze(&self, id: &AccountId) -> Result<(), LedgerError> {
        self.transition(
            id,
            |flags| flags.remove(AccountFlags::FROZEN),
            |account_id, version| LedgerEventKind::AccountUnfrozen {
                account_id,
                version,
            },
        )
        .await
    }

    /// Close an account. Must have no live postings.
    #[instrument(skip(self), name = "ledger.close")]
    pub async fn close(&self, id: &AccountId) -> Result<(), LedgerError> {
        // Emptiness is close's own guard, checked before the transition's
        // write-ahead so a non-empty account records nothing. A closed account
        // holds no live postings, so this ordering still surfaces
        // `AccountAlreadyClosed` (from `transition`) for a re-close.
        if self.has_live_postings(id).await? {
            return Err(LedgerError::AccountNotEmpty(*id));
        }
        self.transition(
            id,
            |flags| {
                *flags |= AccountFlags::CLOSED;
                flags.remove(AccountFlags::FROZEN);
            },
            |account_id, version| LedgerEventKind::AccountClosed {
                account_id,
                version,
            },
        )
        .await
    }

    /// Whether `account` (exact base id and subaccount) has any live posting: one
    /// that is active or reserved by an in-flight saga. Spent postings do not
    /// count. This is the emptiness test [`close`](Self::close) gates on, and the
    /// inflight layer uses it to decide when a drained hold can be closed.
    #[instrument(skip(self), name = "ledger.has_live_postings")]
    pub async fn has_live_postings(&self, account: &AccountId) -> Result<bool, LedgerError> {
        Ok(!self
            .store
            .get_postings_by_account(account.id, Some(account.sub), None, PostingFilter::Live)
            .await?
            .is_empty())
    }
}
