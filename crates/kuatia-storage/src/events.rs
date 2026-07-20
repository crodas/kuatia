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
        /// The account version this transition produced. Pins the event to one
        /// version bump so a recovered transition re-appends idempotently.
        #[serde(default)]
        version: u64,
    },
    /// An account was unfrozen.
    AccountUnfrozen {
        /// The id of the unfrozen account.
        account_id: AccountId,
        /// The account version this transition produced. Pins the event to one
        /// version bump so a recovered transition re-appends idempotently.
        #[serde(default)]
        version: u64,
    },
    /// An account was closed.
    AccountClosed {
        /// The id of the closed account.
        account_id: AccountId,
        /// The account version this transition produced. Pins the event to one
        /// version bump so a recovered transition re-appends idempotently.
        #[serde(default)]
        version: u64,
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
/// (re-appended by crash recovery) carry a key so a second append collapses to
/// the existing row instead of duplicating.
///
/// - A committed transfer keys on its content-addressed id (hex).
/// - An account lifecycle *transition* (freeze/unfreeze/close) keys on the
///   `(account, version)` it records: each version bump happens exactly once, so
///   the pair is a stable identity that recovery reproduces verbatim.
/// - `AccountCreated` has no version-transition identity and is not re-driven, so
///   it returns `None` and may recur.
///
/// Keys are strings so both identities share the store's single `dedup_key`
/// column; the transfer form is the same lowercase hex the column already holds,
/// so no existing row changes meaning.
pub fn event_dedup_key(kind: &LedgerEventKind) -> Option<String> {
    match kind {
        LedgerEventKind::TransferCommitted { transfer_id } => Some(hex(&transfer_id.0)),
        LedgerEventKind::AccountFrozen {
            account_id,
            version,
        }
        | LedgerEventKind::AccountUnfrozen {
            account_id,
            version,
        }
        | LedgerEventKind::AccountClosed {
            account_id,
            version,
        } => Some(format!(
            "acct:{}:{}:{}",
            account_id.id, account_id.sub, version
        )),
        LedgerEventKind::AccountCreated { .. } => None,
    }
}

/// Lower-case hex, matching the SQL backend's `dedup_key` encoding for transfer
/// events so the key a recovered append produces equals the stored one.
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
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
