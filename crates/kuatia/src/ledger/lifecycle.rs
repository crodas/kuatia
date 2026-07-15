//! Account lifecycle: create, freeze, unfreeze, close.
//!
//! Accounts are append-only and versioned: each mutation appends a new version
//! rather than editing in place. Freeze/close guards are validate-time and
//! best-effort under concurrency (see the dumb-storage ADR).

use tracing::instrument;

use kuatia_core::{AccountId, PostingFilter};
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
        let current = self
            .store
            .get_account(id)
            .await
            .map_err(|_| LedgerError::AccountNotFound(*id))?;
        if current.is_closed() {
            return Err(LedgerError::AccountAlreadyClosed(*id));
        }
        let mut next = current.clone();
        next.version = next.version.checked_add(1).ok_or(LedgerError::Overflow)?;
        next.flags |= kuatia_core::AccountFlags::FROZEN;
        self.store.append_account_version(next).await?;
        self.store
            .append_event(&LedgerEvent {
                seq: 0,
                timestamp: now_millis()?,
                kind: LedgerEventKind::AccountFrozen { account_id: *id },
            })
            .await?;
        Ok(())
    }

    /// Unfreeze a previously frozen account.
    #[instrument(skip(self), name = "ledger.unfreeze")]
    pub async fn unfreeze(&self, id: &AccountId) -> Result<(), LedgerError> {
        let current = self
            .store
            .get_account(id)
            .await
            .map_err(|_| LedgerError::AccountNotFound(*id))?;
        if current.is_closed() {
            return Err(LedgerError::AccountAlreadyClosed(*id));
        }
        let mut next = current.clone();
        next.version = next.version.checked_add(1).ok_or(LedgerError::Overflow)?;
        next.flags.remove(kuatia_core::AccountFlags::FROZEN);
        self.store.append_account_version(next).await?;
        self.store
            .append_event(&LedgerEvent {
                seq: 0,
                timestamp: now_millis()?,
                kind: LedgerEventKind::AccountUnfrozen { account_id: *id },
            })
            .await?;
        Ok(())
    }

    /// Close an account. Must have no active postings.
    #[instrument(skip(self), name = "ledger.close")]
    pub async fn close(&self, id: &AccountId) -> Result<(), LedgerError> {
        let current = self
            .store
            .get_account(id)
            .await
            .map_err(|_| LedgerError::AccountNotFound(*id))?;
        if current.is_closed() {
            return Err(LedgerError::AccountAlreadyClosed(*id));
        }
        // Reject if any posting is still live — active or reserved (a transfer
        // in flight). Only spent postings (or none) permit a close.
        if self.has_live_postings(id).await? {
            return Err(LedgerError::AccountNotEmpty(*id));
        }
        let mut next = current.clone();
        next.version = next.version.checked_add(1).ok_or(LedgerError::Overflow)?;
        next.flags |= kuatia_core::AccountFlags::CLOSED;
        next.flags.remove(kuatia_core::AccountFlags::FROZEN);
        self.store.append_account_version(next).await?;
        self.store
            .append_event(&LedgerEvent {
                seq: 0,
                timestamp: now_millis()?,
                kind: LedgerEventKind::AccountClosed { account_id: *id },
            })
            .await?;
        Ok(())
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
