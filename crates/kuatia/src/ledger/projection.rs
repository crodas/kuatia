//! Balance cache points (ADR-0019).
//!
//! Balance is `snapshot + Σ(creates − consumes)` over committed transfers past
//! the closest cache point's commit-time watermark. Summing every one of an
//! account's transfer deltas equals its live-posting sum, so this read always
//! agrees with [`compute_balance`](Ledger::compute_balance). Cache points are
//! append-only: a read takes the one closest to (at or before) now and, once
//! enough new credits/debits have accrued since it, appends a fresh one off the
//! read path. They are a pure optimization that shortens the tail; correctness
//! never depends on them.

use std::collections::HashSet;
use std::sync::Arc;

use tracing::instrument;

use kuatia_core::{AccountId, AssetId, Cent, PostingId};
use kuatia_storage::store::{Store, TransferQuery};

use super::{Ledger, now_millis};
use crate::error::LedgerError;

/// Fold the per-`(account, asset)` delta of a set of committed transfers,
/// `Σ created(+) − Σ consumed(−)` restricted to postings owned by `account` in
/// `asset`, and count how many such postings (credits + debits) were seen.
/// Consumed postings are resolved from the immutable table (the envelope carries
/// only their ids). All arithmetic is checked, in Rust.
async fn fold_account_delta(
    store: &dyn Store,
    account: &AccountId,
    asset: &AssetId,
    records: &[kuatia_storage::store::EnvelopeRecord],
) -> Result<(Cent, u64), LedgerError> {
    let mut delta = Cent::ZERO;
    let mut count: u64 = 0;

    // Created side (credits): the envelope carries owner/asset/value directly.
    for record in records {
        for np in record.envelope.creates() {
            if np.owner == *account && np.asset == *asset {
                delta = delta.checked_add(np.value)?;
                count += 1;
            }
        }
    }

    // Consumed side (debits): gather every consumed id across the window, resolve
    // the postings in one batch, and subtract those owned by this (account, asset).
    let mut consumed_ids: Vec<PostingId> = Vec::new();
    let mut seen: HashSet<PostingId> = HashSet::new();
    for record in records {
        for id in record.envelope.consumes() {
            if seen.insert(*id) {
                consumed_ids.push(*id);
            }
        }
    }
    if !consumed_ids.is_empty() {
        for posting in store.get_postings(&consumed_ids).await? {
            if posting.owner == *account && posting.asset == *asset {
                delta = delta.checked_sub(posting.value)?;
                count += 1;
            }
        }
    }

    Ok((delta, count))
}

/// Load the committed transfers involving `account` with commit time in
/// `[from_ts, to_ts)` (both optional), then fold their `(account, asset)` delta
/// and credit/debit count. `from_ts == None` spans from the beginning;
/// `to_ts == None` spans to the newest committed transfer.
async fn fold_tail(
    store: &dyn Store,
    account: &AccountId,
    asset: &AssetId,
    from_ts: Option<i64>,
    to_ts: Option<i64>,
) -> Result<(Cent, u64), LedgerError> {
    let query = TransferQuery {
        account: Some(account.id),
        sub: Some(account.sub),
        from_ts,
        to_ts,
        ..Default::default()
    };
    let records = store.query_transfers(&query).await?.items;
    fold_account_delta(store, account, asset, &records).await
}

/// Append a fresh cache point for `(account, asset)`: fold the window since the
/// closest cache point's watermark up to `now − grace` onto its snapshot, and
/// append the result. Only appends when that window has at least `min_new`
/// credits/debits, so a hot account that is read constantly does not append a
/// near-duplicate row on every read (`min_new == 0` forces an append). Append-only
/// and best effort; a stale or duplicate append is harmless because a read takes
/// the closest-at-or-before cache point.
///
/// `debounce_ms` is the storage-based single-flight guard. When the newest cache
/// point already sits within `debounce_ms` below the target watermark, this
/// returns before the fold, so a hot account read at high QPS does not fan out a
/// full fold per read. The guard lives in the shared `balance_projection` rows,
/// not in process memory, so it dedups across every ledger instance and survives
/// a restart, and it needs no lease, lock, or CAS (ADR-0019). `debounce_ms == 0`
/// disables it (the reconcile path forces an append regardless).
async fn append_cache_point(
    store: &dyn Store,
    grace_ms: i64,
    min_new: u64,
    debounce_ms: i64,
    account: &AccountId,
    asset: &AssetId,
) -> Result<(), LedgerError> {
    let new_watermark = now_millis()?.saturating_sub(grace_ms);
    let closest = store
        .get_closest_balance_projection(account, asset, new_watermark)
        .await?;
    let (snapshot, from_ts) = match closest {
        // Storage-based debounce: a cache point within `debounce_ms` below the
        // target already shortens the tail enough, so skip the fold. This is what
        // collapses a per-read fan-out into at most one fold per debounce window.
        Some(p) if new_watermark.saturating_sub(p.watermark) < debounce_ms => return Ok(()),
        // A cache point already covers this watermark: nothing to add. (Also the
        // debounce == 0 stop, so an exact-or-newer watermark is never duplicated.)
        Some(p) if p.watermark >= new_watermark => return Ok(()),
        Some(p) => (p.balance, Some(p.watermark.saturating_add(1))),
        None => (Cent::ZERO, None),
    };
    let (fold, count) = fold_tail(
        store,
        account,
        asset,
        from_ts,
        Some(new_watermark.saturating_add(1)),
    )
    .await?;
    // Not enough new activity since the closest cache point to earn a new row.
    if count < min_new {
        return Ok(());
    }
    let balance = snapshot.checked_add(fold)?;
    store
        .append_balance_projection(account, asset, balance, new_watermark)
        .await?;
    Ok(())
}

impl Ledger {
    /// The everyday balance read for one subaccount and asset (ADR-0019): the
    /// closest cache point (at or before now) plus the folded tail of transfers
    /// committed after its watermark. Always equal to
    /// [`compute_balance`](Ledger::compute_balance) at rest, and faster once a
    /// cache point keeps the tail short. With no cache point yet it returns the
    /// authoritative live-posting sum directly (rather than folding the whole
    /// history) and bootstraps a cache point in the background. Once enough
    /// credits/debits have accrued since the closest cache point, it also appends
    /// a new one in the background for later reads.
    #[instrument(skip(self), name = "ledger.balance")]
    pub async fn balance(&self, account: &AccountId, asset: &AssetId) -> Result<Cent, LedgerError> {
        let now = now_millis()?;
        let closest = self
            .store
            .get_closest_balance_projection(account, asset, now)
            .await?;
        match closest {
            Some(p) => {
                let (tail, count) = fold_tail(
                    self.store(),
                    account,
                    asset,
                    Some(p.watermark.saturating_add(1)),
                    None,
                )
                .await?;
                if count >= self.snapshot_interval {
                    self.spawn_append(*account, *asset);
                }
                Ok(p.balance.checked_add(tail)?)
            }
            // No cache point: the authoritative live sum is O(live postings),
            // cheaper than folding the whole history. Bootstrap a cache point in
            // the background (the append itself gates on `snapshot_interval`, so a
            // small account never actually appends) so later reads use the tail.
            None => {
                let balance = self.compute_balance(account, asset).await?;
                self.spawn_append(*account, *asset);
                Ok(balance)
            }
        }
    }

    /// Spawn a best-effort background append gated on `snapshot_interval` new
    /// credits/debits. Uses only the store handle and config, so it needs no
    /// `Arc<Self>` and can run from a `&self` read.
    ///
    /// Redundant spawns are cheap, not suppressed here: the storage-based debounce
    /// inside [`append_cache_point`] (keyed on the newest shared cache-point row,
    /// with `debounce_ms == grace`) returns before the fold when a recent cache
    /// point already exists, so a hot account read at high QPS does at most one
    /// fold per grace window, coordinated across every instance rather than in
    /// this process's memory.
    fn spawn_append(&self, account: AccountId, asset: AssetId) {
        let store = Arc::clone(&self.store);
        let grace = self.projection_grace_ms;
        let min_new = self.snapshot_interval;
        tokio::spawn(async move {
            if let Err(err) =
                append_cache_point(store.as_ref(), grace, min_new, grace, &account, &asset).await
            {
                // Best effort: a failed append only lengthens a later read's tail.
                // Log it so a projection that silently stops advancing is visible.
                tracing::warn!(?account, ?asset, error = %err, "balance projection append failed");
            }
        });
    }

    /// Append a cache point for one `(account, asset)` now (folding up to
    /// `now − grace`), unconditionally. The read path appends lazily in the
    /// background; this forces one (no debounce), exposed for tests and
    /// reconciliation. Append-only: a repeat within the same grace-adjusted
    /// millisecond is a no-op (the watermark is already covered), otherwise it
    /// adds a fresh row.
    pub async fn append_cache_point(
        &self,
        account: &AccountId,
        asset: &AssetId,
    ) -> Result<(), LedgerError> {
        append_cache_point(self.store(), self.projection_grace_ms, 0, 0, account, asset).await
    }
}
