//! Ledger event types and storage trait.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use kuatia_types::{AccountId, EnvelopeId};

use crate::error::StoreError;

/// The kind of ledger event that occurred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LedgerEventKind {
    /// A transfer was committed.
    TransferCommitted {
        /// The content-addressed id of the committed transfer.
        transfer_id: EnvelopeId,
    },
    /// An account was created.
    AccountCreated {
        /// The id of the created account.
        account_id: AccountId,
    },
    /// An account was frozen.
    AccountFrozen {
        /// The id of the frozen account.
        account_id: AccountId,
    },
    /// An account was unfrozen.
    AccountUnfrozen {
        /// The id of the unfrozen account.
        account_id: AccountId,
    },
    /// An account was closed.
    AccountClosed {
        /// The id of the closed account.
        account_id: AccountId,
    },
}

/// A ledger event with a sequence number and timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEvent {
    /// Monotonic sequence number (assigned by the store).
    pub seq: u64,
    /// Unix milliseconds when the event was created.
    pub timestamp: i64,
    /// The event payload.
    pub kind: LedgerEventKind,
}

/// The idempotency key for an event, if it has a natural one. Replayable events
/// (a committed transfer, re-driven by saga recovery) dedup on their transfer
/// id; events with no natural identity (account lifecycle) return `None` and may
/// recur.
pub fn event_dedup_key(kind: &LedgerEventKind) -> Option<EnvelopeId> {
    match kind {
        LedgerEventKind::TransferCommitted { transfer_id } => Some(*transfer_id),
        LedgerEventKind::AccountCreated { .. }
        | LedgerEventKind::AccountFrozen { .. }
        | LedgerEventKind::AccountUnfrozen { .. }
        | LedgerEventKind::AccountClosed { .. } => None,
    }
}

/// Persistent event log for ledger events.
#[async_trait]
pub trait EventStore: Send + Sync {
    /// Append an event and return its sequence number. Idempotent on the event's
    /// [`event_dedup_key`]: appending an event whose key already exists does not
    /// insert a duplicate and returns the existing seq. The `seq` field on the
    /// input is ignored -- the store assigns it.
    async fn append_event(&self, event: &LedgerEvent) -> Result<u64, StoreError>;

    /// Return events with sequence numbers greater than `after_seq`, up to `limit`.
    async fn get_events_since(
        &self,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<LedgerEvent>, StoreError>;
}
