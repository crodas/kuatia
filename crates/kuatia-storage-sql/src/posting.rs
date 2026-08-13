//! [`PostingStore`]: the immutable posting record plus one `live_postings` hot
//! table whose `reservation` column derives each posting's lifecycle state.
//!
//! A posting is in `live_postings` while it is spendable or reserved:
//! `reservation IS NULL` = Active, `reservation = rid` = Reserved by that saga.
//! Once consumed the row is deleted, leaving the posting only in the immutable
//! `postings` table (= Spent). See ADR-0022 (merged hot index) and ADR-0016.

use std::collections::HashMap;

use async_trait::async_trait;
use sqlx::Row;
use sqlx::any::AnyRow;

use kuatia_storage::error::StoreError;
use kuatia_storage::store::*;
use kuatia_types::*;

use crate::SqlStore;
use crate::row::{envelope_id_to_hex, row_to_posting};

/// The `(source_table, state_predicate)` a posting read of the given filter
/// targets. Live/Active/Reserved read the single `live_postings` hot table,
/// narrowed by the `reservation` column; `All` reads the immutable record. No
/// UNION: the live set is one table.
fn filter_source(filter: PostingFilter) -> (&'static str, &'static str) {
    match filter {
        PostingFilter::Active => ("live_postings", " AND reservation IS NULL"),
        PostingFilter::Reserved => ("live_postings", " AND reservation IS NOT NULL"),
        PostingFilter::Live => ("live_postings", ""),
        PostingFilter::All => ("postings", ""),
    }
}

/// Maximum posting ids matched by a single statement. `id_predicate` expands to
/// an `OR` of `n` equality pairs, so the binding constraint is SQLite's
/// expression-tree depth limit (`SQLITE_MAX_EXPR_DEPTH`, default 1000), which a
/// chain of `n` `OR`s reaches at roughly `n` deep. It caps well before the
/// bind-parameter limits (SQLite 32766, PostgreSQL 65535) that `2 * n (+1)`
/// parameters would hit. `500` stays comfortably under the expression-depth
/// limit; callers that pass more ids are chunked, so the id-batch primitives
/// have no practical ceiling on batch size.
pub(crate) const MAX_IDS_PER_QUERY: usize = 500;

/// Build a portable predicate matching a set of posting ids:
/// `(transfer_id = $s AND idx = $s+1) OR (transfer_id = $s+2 AND idx = $s+3) ...`
/// starting at placeholder `$start`. Row-value `IN ((a, b), ...)` is not
/// portable across SQLite and PostgreSQL; an `OR` of equality pairs is. The
/// caller binds each id as `(hex(transfer), idx as i16)` in order, matching the
/// placeholder sequence. `ids` must be non-empty and no longer than
/// [`MAX_IDS_PER_QUERY`]; larger sets are split into chunks by the caller.
pub(crate) fn id_predicate(count: usize, start: u32) -> String {
    (0..count)
        .map(|i| {
            let p = start + (i as u32) * 2;
            format!("(transfer_id = ${} AND idx = ${})", p, p + 1)
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[async_trait]
impl PostingStore for SqlStore {
    async fn get_postings(&self, ids: &[PostingId]) -> Result<Vec<Posting>, StoreError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        // Set-based query per chunk instead of one probe per id, reusing the
        // portable `id_predicate` and binding each id in order as
        // `(hex(transfer), idx as i16)`. Chunked so a large batch never exceeds
        // the backend's bind-parameter limit (see `MAX_IDS_PER_QUERY`).
        let mut found: HashMap<(String, i16), Posting> = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(MAX_IDS_PER_QUERY) {
            let sql = format!(
                "SELECT * FROM postings WHERE {}",
                id_predicate(chunk.len(), 1)
            );
            let mut q = sqlx::query(&sql);
            for id in chunk {
                q = q
                    .bind(envelope_id_to_hex(&id.transfer))
                    .bind(id.index as i16);
            }
            let rows = q
                .fetch_all(&self.pool)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;

            // Index the fetched postings by the same `(hex, idx)` key that was bound.
            for row in &rows {
                let posting = row_to_posting(row)?;
                let key = (
                    envelope_id_to_hex(&posting.id.transfer),
                    posting.id.index as i16,
                );
                found.insert(key, posting);
            }
        }

        // Return in input order, erroring on the first id absent from the batch
        // (matching the per-id lookup's `NotFound` semantics).
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            let key = (envelope_id_to_hex(&id.transfer), id.index as i16);
            let posting = found
                .get(&key)
                .ok_or_else(|| StoreError::NotFound(format!("posting {id:?}")))?;
            result.push(posting.clone());
        }
        Ok(result)
    }

    async fn get_postings_by_account(
        &self,
        id: i64,
        sub: Option<i64>,
        asset: Option<&AssetId>,
        filter: PostingFilter,
    ) -> Result<Vec<Posting>, StoreError> {
        // Build the predicate dynamically: `sub == None` spans every subaccount
        // of `id`, `Some(s)` restricts to one. The subaccount is compared only
        // for equality, never as a magnitude. The filter picks the source table
        // and, for the live table, the `reservation` state predicate.
        let (source, state) = filter_source(filter);
        let mut sql = format!("SELECT * FROM {source} WHERE owner = $1");
        let mut placeholder = 2u32;
        if sub.is_some() {
            sql.push_str(&format!(" AND subaccount = ${placeholder}"));
            placeholder += 1;
        }
        if asset.is_some() {
            sql.push_str(&format!(" AND asset = ${placeholder}"));
        }
        sql.push_str(state);
        // Deterministic order by the posting primary key, matching
        // `query_postings`, so callers (and pagination built on top) see a
        // stable sequence.
        sql.push_str(" ORDER BY transfer_id, idx");

        let mut q = sqlx::query(&sql).bind(id);
        if let Some(s) = sub {
            q = q.bind(s);
        }
        if let Some(a) = asset {
            q = q.bind(a.0 as i32);
        }

        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        rows.iter().map(row_to_posting).collect()
    }

    async fn get_posting_states(&self, ids: &[PostingId]) -> Result<Vec<PostingState>, StoreError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        // Two set-based queries per chunk (was three, before the hot tables were
        // merged): the live table carries both Active (`reservation IS NULL`) and
        // Reserved (`reservation = rid`) in one row, and `postings` decides Spent
        // vs Missing for ids absent from the live set. Reuses the portable
        // `id_predicate` (an OR of equality pairs; row-value `IN` is not portable
        // across SQLite and PostgreSQL), binding each id as `(hex, idx)`.

        // Key membership by the same `(hex, idx)` values that were bound, so the
        // per-id lookup below matches without decoding transfer ids back.
        let row_key = |row: &AnyRow| -> Result<(String, i16), StoreError> {
            let transfer_id: String = row
                .try_get("transfer_id")
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            let idx: i16 = row
                .try_get("idx")
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            Ok((transfer_id, idx))
        };

        // `live` maps each present id to its reservation: `None` = Active,
        // `Some(rid)` = Reserved. `present` is every id in the immutable record,
        // used to tell Spent (present, not live) from Missing (absent).
        let mut live: HashMap<(String, i16), Option<i64>> = HashMap::new();
        let mut present: std::collections::HashSet<(String, i16)> =
            std::collections::HashSet::new();

        for chunk in ids.chunks(MAX_IDS_PER_QUERY) {
            let predicate = id_predicate(chunk.len(), 1);

            let live_sql = format!(
                "SELECT transfer_id, idx, reservation FROM live_postings WHERE {predicate}"
            );
            let mut live_q = sqlx::query(&live_sql);
            for id in chunk {
                live_q = live_q
                    .bind(envelope_id_to_hex(&id.transfer))
                    .bind(id.index as i16);
            }
            let live_rows = live_q
                .fetch_all(&self.pool)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            for row in &live_rows {
                // `reservation` is nullable: NULL decodes to `None` = Active.
                let rid: Option<i64> = row
                    .try_get("reservation")
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                live.insert(row_key(row)?, rid);
            }

            let present_sql = format!("SELECT transfer_id, idx FROM postings WHERE {predicate}");
            let mut present_q = sqlx::query(&present_sql);
            for id in chunk {
                present_q = present_q
                    .bind(envelope_id_to_hex(&id.transfer))
                    .bind(id.index as i16);
            }
            let present_rows = present_q
                .fetch_all(&self.pool)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            for row in &present_rows {
                present.insert(row_key(row)?);
            }
        }

        // Reconstruct each id's state in input order, preserving the active >
        // reserved > spent > missing precedence of the original probes.
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let key = (envelope_id_to_hex(&id.transfer), id.index as i16);
            out.push(match live.get(&key) {
                Some(None) => PostingState::Active,
                Some(Some(rid)) => PostingState::Reserved(ReservationId::new(*rid)),
                None if present.contains(&key) => PostingState::Spent,
                None => PostingState::Missing,
            });
        }
        Ok(out)
    }

    async fn query_postings(&self, query: &PostingQuery) -> Result<Page<Posting>, StoreError> {
        let (where_clause, count_clause) = {
            let (source, state) = filter_source(query.filter);
            let mut w = String::from("WHERE owner = $1");
            let mut idx = 2u32;
            if query.sub.is_some() {
                w.push_str(&format!(" AND subaccount = ${idx}"));
                idx += 1;
            }
            if query.asset.is_some() {
                w.push_str(&format!(" AND asset = ${idx}"));
            }
            w.push_str(state);
            let c = format!("SELECT COUNT(*) as cnt FROM {source} {w}");
            let (offset, limit) = kuatia_storage::query::window(query.offset, query.limit);
            // Order by the posting primary key so pagination is deterministic:
            // without it LIMIT/OFFSET could skip or repeat rows across pages.
            w.push_str(&format!(
                " ORDER BY transfer_id, idx LIMIT {limit} OFFSET {offset}"
            ));
            (format!("SELECT * FROM {source} {w}"), c)
        };

        // Build count query
        let mut count_q = sqlx::query(&count_clause).bind(query.account);
        if let Some(s) = query.sub {
            count_q = count_q.bind(s);
        }
        if let Some(ref a) = query.asset {
            count_q = count_q.bind(a.0 as i32);
        }
        let count_row = count_q
            .fetch_one(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let total: i64 = count_row
            .try_get("cnt")
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        // Build data query
        let mut data_q = sqlx::query(&where_clause).bind(query.account);
        if let Some(s) = query.sub {
            data_q = data_q.bind(s);
        }
        if let Some(ref a) = query.asset {
            data_q = data_q.bind(a.0 as i32);
        }
        let rows = data_q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        let items: Vec<Posting> = rows.iter().map(row_to_posting).collect::<Result<_, _>>()?;
        Ok(Page {
            items,
            total: total as u64,
        })
    }

    async fn reserve_postings(
        &self,
        ids: &[PostingId],
        reservation: ReservationId,
    ) -> Result<u64, StoreError> {
        // Dumb instruction over the whole id set: flip each still-Active row's
        // `reservation` from NULL to this saga's id. `WHERE reservation IS NULL`
        // is the atomic single-winner claim: concurrent reserves serialize on the
        // row lock, and the loser's predicate no longer matches, so exactly one
        // wins each contended id. `rows_affected` is the number claimed; an
        // already-reserved or spent id does not match and is not counted.
        if ids.is_empty() {
            return Ok(0);
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        // Chunked so a large id set stays under the bind-parameter limit; all
        // chunks share one transaction so the whole claim is atomic.
        let mut claimed: u64 = 0;
        for chunk in ids.chunks(MAX_IDS_PER_QUERY) {
            // Reservation is $1; each id pair follows starting at $2.
            let sql = format!(
                "UPDATE live_postings SET reservation = $1 WHERE ({}) AND reservation IS NULL",
                id_predicate(chunk.len(), 2)
            );
            let mut q = sqlx::query(&sql).bind(reservation.0);
            for id in chunk {
                q = q
                    .bind(envelope_id_to_hex(&id.transfer))
                    .bind(id.index as i16);
            }
            let res = q
                .execute(&mut *tx)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            claimed += res.rows_affected();
        }

        tx.commit()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(claimed)
    }

    async fn release_postings(
        &self,
        ids: &[PostingId],
        reservation: ReservationId,
    ) -> Result<u64, StoreError> {
        // Dumb instruction over the whole id set: clear the `reservation` of the
        // rows this saga holds, returning them to Active. `rows_affected` is the
        // number released; an id already Active or reserved by another saga does
        // not match `reservation = rid` and is left untouched.
        if ids.is_empty() {
            return Ok(0);
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        let mut released: u64 = 0;
        for chunk in ids.chunks(MAX_IDS_PER_QUERY) {
            // Reservation is $1; each id pair follows starting at $2.
            let sql = format!(
                "UPDATE live_postings SET reservation = NULL WHERE ({}) AND reservation = $1",
                id_predicate(chunk.len(), 2)
            );
            let mut q = sqlx::query(&sql).bind(reservation.0);
            for id in chunk {
                q = q
                    .bind(envelope_id_to_hex(&id.transfer))
                    .bind(id.index as i16);
            }
            let res = q
                .execute(&mut *tx)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            released += res.rows_affected();
        }

        tx.commit()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(released)
    }

    async fn deactivate_postings(
        &self,
        ids: &[PostingId],
        reservation: Option<ReservationId>,
    ) -> Result<u64, StoreError> {
        // Dumb instruction over the whole id set: DELETE the ids from the live
        // table so they become spent (present only in the immutable `postings`).
        // The raw path removes still-Active rows (`reservation IS NULL`); the saga
        // path removes only the rows reserved by `rid`. `rows_affected` is the
        // count; the caller interprets a shortfall. Chunked under one transaction.
        if ids.is_empty() {
            return Ok(0);
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let mut removed: u64 = 0;
        for chunk in ids.chunks(MAX_IDS_PER_QUERY) {
            let (sql, rid) = match reservation {
                // Raw path: remove still-Active rows.
                None => (
                    format!(
                        "DELETE FROM live_postings WHERE ({}) AND reservation IS NULL",
                        id_predicate(chunk.len(), 1)
                    ),
                    None,
                ),
                // Saga path: remove only the rows reserved by `rid`.
                Some(rid) => (
                    format!(
                        "DELETE FROM live_postings WHERE ({}) AND reservation = $1",
                        id_predicate(chunk.len(), 2)
                    ),
                    Some(rid),
                ),
            };
            let mut q = sqlx::query(&sql);
            if let Some(rid) = rid {
                q = q.bind(rid.0);
            }
            for id in chunk {
                q = q
                    .bind(envelope_id_to_hex(&id.transfer))
                    .bind(id.index as i16);
            }
            let res = q
                .execute(&mut *tx)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            removed += res.rows_affected();
        }
        tx.commit()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(removed)
    }

    async fn insert_postings(&self, postings: &[Posting]) -> Result<u64, StoreError> {
        // Dumb instruction: insert each posting into the immutable table and, only
        // when the row was newly inserted, add it to the live table as Active
        // (reservation NULL). Return the count of immutable rows inserted. The
        // newness gate stops a replayed finalize from re-activating a since-spent
        // posting.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let mut inserted: u64 = 0;
        for posting in postings {
            let hex = envelope_id_to_hex(&posting.id.transfer);
            let res = sqlx::query(
                "INSERT INTO postings (transfer_id, idx, owner, subaccount, asset, value) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (transfer_id, idx) DO NOTHING"
            )
                .bind(hex.clone())
                .bind(posting.id.index as i16)
                .bind(posting.owner.id)
                .bind(posting.owner.sub)
                .bind(posting.asset.0 as i32)
                .bind(posting.value.to_string())
                .execute(&mut *tx)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            if res.rows_affected() == 1 {
                // Activate a full copy (reservation NULL) so spendable reads never merge.
                sqlx::query(
                    "INSERT INTO live_postings (transfer_id, idx, owner, subaccount, asset, value, reservation) VALUES ($1, $2, $3, $4, $5, $6, NULL) ON CONFLICT (transfer_id, idx) DO NOTHING",
                )
                .bind(hex)
                .bind(posting.id.index as i16)
                .bind(posting.owner.id)
                .bind(posting.owner.sub)
                .bind(posting.asset.0 as i32)
                .bind(posting.value.to_string())
                .execute(&mut *tx)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
                inserted += 1;
            }
        }
        tx.commit()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(inserted)
    }
}
