//! [`PostingStore`]: the immutable posting record plus two hot index tables
//! (`active_postings`, `reserved_postings`) whose membership derives each
//! posting's lifecycle state.
//!
//! A posting is in `active_postings` while spendable, moves to
//! `reserved_postings` (carrying its reservation) while claimed by a saga, and
//! once consumed is deleted from both, leaving it only in the immutable
//! `postings` table (= Spent). See ADR-0016.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use sqlx::Row;
use sqlx::any::AnyRow;

use kuatia_storage::error::StoreError;
use kuatia_storage::store::*;
use kuatia_types::*;

use crate::SqlStore;
use crate::row::{envelope_id_to_hex, row_to_posting};

/// The FROM source for a posting read of the given derived state. Each index
/// table carries a full row copy, so the live-set reads target the index table
/// directly with no merge back to the immutable `postings` record. `Live` is a
/// `UNION ALL` of the two disjoint live sets (the shared 6 data columns), still
/// with no join to history. Portable across SQLite and PostgreSQL.
fn filter_source(filter: PostingFilter) -> &'static str {
    match filter {
        PostingFilter::Active => "active_postings",
        PostingFilter::Reserved => "reserved_postings",
        PostingFilter::All => "postings",
        PostingFilter::Live => {
            "(SELECT transfer_id, idx, owner, subaccount, asset, value FROM active_postings \
             UNION ALL \
             SELECT transfer_id, idx, owner, subaccount, asset, value FROM reserved_postings) AS live"
        }
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
const MAX_IDS_PER_QUERY: usize = 500;

/// Build a portable predicate matching a set of posting ids:
/// `(transfer_id = $s AND idx = $s+1) OR (transfer_id = $s+2 AND idx = $s+3) ...`
/// starting at placeholder `$start`. Row-value `IN ((a, b), ...)` is not
/// portable across SQLite and PostgreSQL; an `OR` of equality pairs is. The
/// caller binds each id as `(hex(transfer), idx as i16)` in order, matching the
/// placeholder sequence. `ids` must be non-empty and no longer than
/// [`MAX_IDS_PER_QUERY`]; larger sets are split into chunks by the caller.
fn id_predicate(count: usize, start: u32) -> String {
    (0..count)
        .map(|i| {
            let p = start + (i as u32) * 2;
            format!("(transfer_id = ${} AND idx = ${})", p, p + 1)
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

// ---------------------------------------------------------------------------
// PostingStore
// ---------------------------------------------------------------------------

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
        // for equality, never as a magnitude. The derived-state filter selects
        // which table (index copy or immutable record) to read from directly.
        let mut sql = format!("SELECT * FROM {} WHERE owner = $1", filter_source(filter));
        let mut placeholder = 2u32;
        if sub.is_some() {
            sql.push_str(&format!(" AND subaccount = ${placeholder}"));
            placeholder += 1;
        }
        if asset.is_some() {
            sql.push_str(&format!(" AND asset = ${placeholder}"));
        }
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

        // One set-based query per state table instead of up to three probes per
        // id, reusing the portable `id_predicate` (an OR of equality pairs;
        // row-value `IN` is not portable across SQLite and PostgreSQL) and
        // binding every id in order as `(hex(transfer), idx as i16)`. Chunked so
        // a large batch never exceeds the bind-parameter limit.

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

        let mut active: HashSet<(String, i16)> = HashSet::new();
        let mut reserved: HashMap<(String, i16), i64> = HashMap::new();
        let mut spent: HashSet<(String, i16)> = HashSet::new();

        for chunk in ids.chunks(MAX_IDS_PER_QUERY) {
            let predicate = id_predicate(chunk.len(), 1);

            let active_sql =
                format!("SELECT transfer_id, idx FROM active_postings WHERE {predicate}");
            let mut active_q = sqlx::query(&active_sql);
            for id in chunk {
                active_q = active_q
                    .bind(envelope_id_to_hex(&id.transfer))
                    .bind(id.index as i16);
            }
            let active_rows = active_q
                .fetch_all(&self.pool)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            for row in &active_rows {
                active.insert(row_key(row)?);
            }

            let reserved_sql = format!(
                "SELECT transfer_id, idx, reservation FROM reserved_postings WHERE {predicate}"
            );
            let mut reserved_q = sqlx::query(&reserved_sql);
            for id in chunk {
                reserved_q = reserved_q
                    .bind(envelope_id_to_hex(&id.transfer))
                    .bind(id.index as i16);
            }
            let reserved_rows = reserved_q
                .fetch_all(&self.pool)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            for row in &reserved_rows {
                let rid: i64 = row
                    .try_get("reservation")
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                reserved.insert(row_key(row)?, rid);
            }

            let spent_sql = format!("SELECT transfer_id, idx FROM postings WHERE {predicate}");
            let mut spent_q = sqlx::query(&spent_sql);
            for id in chunk {
                spent_q = spent_q
                    .bind(envelope_id_to_hex(&id.transfer))
                    .bind(id.index as i16);
            }
            let spent_rows = spent_q
                .fetch_all(&self.pool)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            for row in &spent_rows {
                spent.insert(row_key(row)?);
            }
        }

        // Reconstruct each id's state in input order, preserving the active >
        // reserved > spent > missing precedence of the original probes.
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let key = (envelope_id_to_hex(&id.transfer), id.index as i16);
            out.push(if active.contains(&key) {
                PostingState::Active
            } else if let Some(rid) = reserved.get(&key) {
                PostingState::Reserved(ReservationId::new(*rid))
            } else if spent.contains(&key) {
                PostingState::Spent
            } else {
                PostingState::Missing
            });
        }
        Ok(out)
    }

    async fn query_postings(&self, query: &PostingQuery) -> Result<Page<Posting>, StoreError> {
        let (where_clause, count_clause) = {
            let source = filter_source(query.filter);
            let mut w = String::from("WHERE owner = $1");
            let mut idx = 2u32;
            if query.sub.is_some() {
                w.push_str(&format!(" AND subaccount = ${idx}"));
                idx += 1;
            }
            if query.asset.is_some() {
                w.push_str(&format!(" AND asset = ${idx}"));
            }
            let c = format!("SELECT COUNT(*) as cnt FROM {source} {w}");
            let limit = query.limit.unwrap_or(u32::MAX);
            let offset = query.offset.unwrap_or(0);
            // Order by the posting primary key so pagination is deterministic:
            // without it LIMIT/OFFSET could skip or repeat rows across pages,
            // especially for `Live`, whose source is a `UNION ALL` with no
            // inherent order.
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
        // Dumb instruction over the whole id set, in two statements: copy the
        // currently-active rows into the reserved index (sourced from
        // `active_postings`, so only active ids move), then delete those same
        // ids from `active_postings`. The DELETE's affected count is the number
        // claimed, and by active/reserved disjointness it equals the INSERT's
        // row count. Concurrent reserves serialize on the reserved-index primary
        // key, so exactly one wins each contended id.
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
            let insert_sql = format!(
                "INSERT INTO reserved_postings (transfer_id, idx, owner, subaccount, asset, value, reservation) \
                 SELECT transfer_id, idx, owner, subaccount, asset, value, $1 FROM active_postings WHERE {} \
                 ON CONFLICT (transfer_id, idx) DO NOTHING",
                id_predicate(chunk.len(), 2)
            );
            let mut insert_q = sqlx::query(&insert_sql).bind(reservation.0);
            for id in chunk {
                insert_q = insert_q
                    .bind(envelope_id_to_hex(&id.transfer))
                    .bind(id.index as i16);
            }
            insert_q
                .execute(&mut *tx)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;

            let delete_sql = format!(
                "DELETE FROM active_postings WHERE {}",
                id_predicate(chunk.len(), 1)
            );
            let mut delete_q = sqlx::query(&delete_sql);
            for id in chunk {
                delete_q = delete_q
                    .bind(envelope_id_to_hex(&id.transfer))
                    .bind(id.index as i16);
            }
            let del = delete_q
                .execute(&mut *tx)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            claimed += del.rows_affected();
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
        // Dumb instruction over the whole id set: copy the rows reserved by
        // `reservation` back into the active index, then delete them from the
        // reserved index. The DELETE's affected count is the number released; an
        // id already active or reserved by another saga does not match.
        if ids.is_empty() {
            return Ok(0);
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        // Chunked so a large id set stays under the bind-parameter limit; all
        // chunks share one transaction.
        let mut released: u64 = 0;
        for chunk in ids.chunks(MAX_IDS_PER_QUERY) {
            // Reservation is $1; each id pair follows starting at $2.
            let insert_sql = format!(
                "INSERT INTO active_postings (transfer_id, idx, owner, subaccount, asset, value) \
                 SELECT transfer_id, idx, owner, subaccount, asset, value FROM reserved_postings \
                 WHERE ({}) AND reservation = $1 ON CONFLICT (transfer_id, idx) DO NOTHING",
                id_predicate(chunk.len(), 2)
            );
            let mut insert_q = sqlx::query(&insert_sql).bind(reservation.0);
            for id in chunk {
                insert_q = insert_q
                    .bind(envelope_id_to_hex(&id.transfer))
                    .bind(id.index as i16);
            }
            insert_q
                .execute(&mut *tx)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;

            let delete_sql = format!(
                "DELETE FROM reserved_postings WHERE ({}) AND reservation = $1",
                id_predicate(chunk.len(), 2)
            );
            let mut delete_q = sqlx::query(&delete_sql).bind(reservation.0);
            for id in chunk {
                delete_q = delete_q
                    .bind(envelope_id_to_hex(&id.transfer))
                    .bind(id.index as i16);
            }
            let del = delete_q
                .execute(&mut *tx)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            released += del.rows_affected();
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
        // Dumb instruction over the whole id set: a DELETE removes the ids from
        // an index so they become spent (present only in the immutable table).
        // `rows_affected` is the count; the caller interprets a shortfall.
        // Chunked under one transaction so a large id set stays within the
        // bind-parameter limit while the removal stays atomic.
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
                // Raw path: remove from the active index.
                None => (
                    format!(
                        "DELETE FROM active_postings WHERE {}",
                        id_predicate(chunk.len(), 1)
                    ),
                    None,
                ),
                // Saga path: remove only the rows reserved by `rid`.
                Some(rid) => (
                    format!(
                        "DELETE FROM reserved_postings WHERE ({}) AND reservation = $1",
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
        // when the row was newly inserted, add its id to the active index. Return
        // the count of immutable rows inserted. The newness gate stops a replayed
        // finalize from re-activating a since-spent posting.
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
                // Activate a full copy so spendable reads never merge.
                sqlx::query(
                    "INSERT INTO active_postings (transfer_id, idx, owner, subaccount, asset, value) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (transfer_id, idx) DO NOTHING",
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
