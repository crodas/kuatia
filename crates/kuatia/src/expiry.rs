//! Hold expiry: an in-memory deadline index and the background reaper that
//! auto-voids inflight holds once their deadline passes.
//!
//! The deadline itself is a durable fact recorded in each authorize transfer's
//! metadata (see [`crate::inflight`]). This module keeps a derived
//! `BTreeMap<deadline, {handle}>` on the [`Ledger`], rebuilt from that metadata
//! on [`Ledger::recover`], and runs a task that sleeps until the earliest
//! deadline and returns due holds to their funders via the ordinary
//! [`Ledger::void`] path. See `doc/adr/0016-hold-expiry-and-reaper.md`.

use std::sync::Arc;
use std::time::Duration;

use kuatia_core::EnvelopeId;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::error::LedgerError;
use crate::ledger::{Ledger, now_millis};

/// Handle to a spawned expiry reaper. Dropping it aborts the task, so hold it for
/// as long as the ledger should auto-void expired holds.
#[derive(Debug)]
pub struct ReaperHandle {
    task: JoinHandle<()>,
}

impl ReaperHandle {
    /// Stop the reaper. Equivalent to dropping the handle.
    pub fn stop(self) {
        self.task.abort();
    }
}

impl Drop for ReaperHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Ledger {
    /// Record `inflight`'s auto-void deadline in the in-memory index and wake the
    /// reaper (in case this deadline is earlier than the one it is sleeping on).
    pub(crate) fn register_expiry(&self, inflight: EnvelopeId, expires_at: i64) {
        // The map is only ever locked for these tiny, await-free critical
        // sections, so a poisoned lock is unreachable in practice; fall back to
        // the inner value rather than panicking if it ever happens.
        let mut idx = self
            .expiry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        idx.entry(expires_at).or_default().insert(inflight);
        drop(idx);
        self.reaper_wake.notify_one();
    }

    /// Drop `inflight` from the deadline index. Called when a hold settles
    /// (confirm_all / void) so the reaper skips it. A missing entry is a no-op; a
    /// stale entry left behind is harmless (the reaper's void would be a no-op).
    pub(crate) fn deregister_expiry(&self, inflight: &EnvelopeId) {
        let mut idx = self
            .expiry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        idx.retain(|_, handles| {
            handles.remove(inflight);
            !handles.is_empty()
        });
    }

    /// Rebuild the deadline index from the durable authorize metadata, replacing
    /// whatever was there. Called by [`recover`](Ledger::recover) on startup so
    /// deadlines set before a restart still drive the reaper.
    pub async fn rebuild_expiry_index(self: &Arc<Self>) -> Result<(), LedgerError> {
        let open = self.open_inflights_with_expiry().await?;
        {
            let mut idx = self
                .expiry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            idx.clear();
            for (handle, at) in open {
                idx.entry(at).or_default().insert(handle);
            }
        }
        self.reaper_wake.notify_one();
        Ok(())
    }

    /// The earliest registered deadline, if any.
    fn next_deadline(&self) -> Option<i64> {
        self.expiry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .next()
            .copied()
    }

    /// Remove and return every handle whose deadline is at or before `now`.
    fn take_due(&self, now: i64) -> Vec<EnvelopeId> {
        let mut idx = self
            .expiry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Split off everything strictly after `now`; keep the due remainder.
        let future = idx.split_off(&(now + 1));
        let due: Vec<EnvelopeId> = idx.values().flat_map(|s| s.iter().copied()).collect();
        *idx = future;
        due
    }

    /// Void every hold whose deadline has passed, returning how many were
    /// processed. Each is removed from the index first (so a failing void does not
    /// spin the reaper), then voided via the ordinary path. A void that fails
    /// (e.g. a funder was closed) is logged; the hold reappears on the next
    /// [`rebuild_expiry_index`](Ledger::rebuild_expiry_index) if still open.
    pub async fn expire_due(self: &Arc<Self>, now: i64) -> usize {
        let due = self.take_due(now);
        for handle in &due {
            if let Err(e) = self.void(handle).await {
                warn!(inflight = ?handle, error = %e, "expiry reaper failed to void hold");
            }
        }
        due.len()
    }

    /// Spawn the background reaper. It sleeps until the earliest deadline, voids
    /// the holds due at that point, and repeats, waking early whenever a newer,
    /// earlier deadline is registered. Returns a [`ReaperHandle`]; drop it (or call
    /// [`ReaperHandle::stop`]) to end the task.
    ///
    /// Run at most one reaper per store. Two racing reapers are safe (void is
    /// idempotent; the loser settles nothing) but redundant.
    pub fn spawn_expiry_reaper(self: &Arc<Self>) -> ReaperHandle {
        let ledger = Arc::clone(self);
        let task = tokio::spawn(async move {
            loop {
                match ledger.next_deadline() {
                    // Nothing scheduled: wait until a deadline is registered.
                    None => ledger.reaper_wake.notified().await,
                    Some(at) => {
                        let now = match now_millis() {
                            Ok(n) => n,
                            // Unreachable in practice (clock before the Unix
                            // epoch); wait for a wake rather than busy-looping.
                            Err(_) => {
                                ledger.reaper_wake.notified().await;
                                continue;
                            }
                        };
                        if at <= now {
                            ledger.expire_due(now).await;
                        } else {
                            // Sleep until the deadline, but wake early if a newer,
                            // earlier deadline arrives. A registration that lands in
                            // this gap stores a Notify permit, so it is not lost.
                            let wait = Duration::from_millis((at - now) as u64);
                            tokio::select! {
                                _ = tokio::time::sleep(wait) => {}
                                _ = ledger.reaper_wake.notified() => {}
                            }
                        }
                    }
                }
            }
        });
        ReaperHandle { task }
    }
}
