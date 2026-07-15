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

/// Cut a fully-filtered, ordered record set into one page: `total` is the
/// pre-pagination count, then skip `offset` (default 0) and take `limit`
/// (default unbounded).
///
/// Callers that push `LIMIT`/`OFFSET` into the store (e.g. SQL `query_postings`)
/// build their own [`Page`] and skip this; it exists for the backends that hold
/// the full candidate set in memory.
pub fn paginate<T>(records: Vec<T>, offset: Option<u32>, limit: Option<u32>) -> Page<T> {
    let total = records.len() as u64;
    let offset = offset.unwrap_or(0) as usize;
    let limit = limit.unwrap_or(u32::MAX) as usize;
    let items = records.into_iter().skip(offset).take(limit).collect();
    Page { items, total }
}
