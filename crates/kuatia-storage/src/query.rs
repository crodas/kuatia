//! Filter and pagination primitives shared by every [`Store`](crate::store::Store)
//! backend, stated once above the trait seam.
//!
//! A backend's `query_transfers`/`query_postings` implementation does only what
//! is genuinely backend-specific: load the candidate records (via an account
//! index, a store-wide scan, or a SQL `LIMIT` push-down). Everything after that,
//! the time-window/book predicate and the `total` + `skip`/`take` cut, is the
//! same contract regardless of backend and lives here.

use crate::store::{EnvelopeRecord, Page, TransferQuery};

/// Keep only the transfers matching a query's time-window and book predicates.
///
/// The account/subaccount filter is *not* applied here: a backend narrows to
/// participating accounts when it loads candidates (an in-memory participation
/// index or the SQL `transfer_accounts` join), because that filter is what the
/// backend can push down. This covers every remaining predicate so both
/// backends agree on the contract.
pub fn filter_transfers(
    records: Vec<EnvelopeRecord>,
    query: &TransferQuery,
) -> Vec<EnvelopeRecord> {
    records
        .into_iter()
        .filter(|r| {
            if let Some(from) = query.from_ts
                && r.created_at < from
            {
                return false;
            }
            if let Some(to) = query.to_ts
                && r.created_at >= to
            {
                return false;
            }
            if let Some(book) = query.book
                && r.envelope.book() != book
            {
                return false;
            }
            true
        })
        .collect()
}

/// The effective `(offset, limit)` window from a query's optional bounds: offset
/// defaults to 0, limit to unbounded. One definition shared by the in-memory
/// [`paginate`] cut and the SQL `LIMIT`/`OFFSET` push-down, so the two agree on
/// what an absent bound means instead of each restating the defaults.
pub fn window(offset: Option<u32>, limit: Option<u32>) -> (u32, u32) {
    (offset.unwrap_or(0), limit.unwrap_or(u32::MAX))
}

/// Cut a fully-filtered, ordered record set into one page: `total` is the
/// pre-pagination count (see [`Page`]), then apply the [`window`] (skip `offset`,
/// take `limit`).
///
/// Callers that push `LIMIT`/`OFFSET` into the store (e.g. SQL `query_postings`)
/// build their own [`Page`] and skip this; it exists for the backends that hold
/// the full candidate set in memory.
pub fn paginate<T>(records: Vec<T>, offset: Option<u32>, limit: Option<u32>) -> Page<T> {
    let total = records.len() as u64;
    let (offset, limit) = window(offset, limit);
    let items = records
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect();
    Page { items, total }
}
