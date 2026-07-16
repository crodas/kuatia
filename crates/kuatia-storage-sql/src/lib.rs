//! SQL-backed Store implementation for SQLite and PostgreSQL.
//!
//! Uses `sqlx::Any` for database-agnostic queries. Enable features
//! `sqlite` or `postgres` to select the backend.
//!
//! ```text
//! let pool = sqlx::any::Pool<Any>Options::new()
//!     .connect("sqlite::memory:").await?;
//! let store = SqlStore::new(pool);
//! store.migrate().await?;
//! ```

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::atomic::{AtomicU8, Ordering};

use async_trait::async_trait;
use sqlx::{Any, Pool, Row};

use kuatia_storage::error::StoreError;
use kuatia_storage::events::{EventStore, LedgerEvent};
use kuatia_storage::query::{filter_transfers, paginate};
use kuatia_storage::store::*;
use kuatia_types::*;

// Cached backend kind for `SqlStore::backend`.
const BACKEND_UNKNOWN: u8 = 0;
const BACKEND_POSTGRES: u8 = 1;
const BACKEND_SQLITE: u8 = 2;

/// Row-locking clause appended to a `SELECT` on backends that support it
/// (PostgreSQL). SQLite has no `FOR UPDATE` and serializes writers itself, so it
/// gets an empty clause.
const FOR_UPDATE: &str = " FOR UPDATE";

/// SQL-backed [`Store`] implementation.
pub struct SqlStore {
    pool: Pool<Any>,
    autoid: kuatia_types::autoid::AutoId,
    /// Detected backend kind (lazily probed): one of `BACKEND_*`.
    backend: AtomicU8,
}

impl SqlStore {
    /// Create a new SQL store wrapping an existing connection pool.
    pub fn new(pool: Pool<Any>) -> Self {
        Self {
            pool,
            autoid: kuatia_types::autoid::AutoId::new(),
            backend: AtomicU8::new(BACKEND_UNKNOWN),
        }
    }

    /// Whether the backend is PostgreSQL. Probed once and cached: `SELECT
    /// sqlite_version()` succeeds only on SQLite, so a failure means Postgres.
    async fn is_postgres(&self) -> Result<bool, StoreError> {
        match self.backend.load(Ordering::Relaxed) {
            BACKEND_POSTGRES => return Ok(true),
            BACKEND_SQLITE => return Ok(false),
            _ => {}
        }
        let is_sqlite = sqlx::query("SELECT sqlite_version()")
            .fetch_optional(&self.pool)
            .await
            .is_ok();
        self.backend.store(
            if is_sqlite {
                BACKEND_SQLITE
            } else {
                BACKEND_POSTGRES
            },
            Ordering::Relaxed,
        );
        Ok(!is_sqlite)
    }

    /// The row-locking clause for the current backend: [`FOR_UPDATE`] on
    /// Postgres, empty on SQLite.
    async fn lock_clause(&self) -> Result<&'static str, StoreError> {
        Ok(if self.is_postgres().await? {
            FOR_UPDATE
        } else {
            ""
        })
    }

    /// Run database migrations. Idempotent: a `_migrations` ledger records what
    /// has been applied, so re-running is a no-op. Every column is a text type,
    /// so the store holds no opaque binary and the DDL is identical for both
    /// backends. Content-addressed ids and opaque saga bytes are stored as hex
    /// `TEXT`, and JSON payloads as their `TEXT` serialization, keeping every
    /// row legible for auditing.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        sqlx::query("CREATE TABLE IF NOT EXISTS _migrations (name TEXT PRIMARY KEY)")
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        let migrations: &[(&str, &str)] = &[
            ("001_init", include_str!("migrations/001_init.sql")),
            (
                "002_subaccounts",
                include_str!("migrations/002_subaccounts.sql"),
            ),
            (
                "003_drop_user_data",
                include_str!("migrations/003_drop_user_data.sql"),
            ),
            (
                "004_index_tables",
                include_str!("migrations/004_index_tables.sql"),
            ),
            (
                "005_account_head",
                include_str!("migrations/005_account_head.sql"),
            ),
            (
                "006_drop_policy",
                include_str!("migrations/006_drop_policy.sql"),
            ),
        ];

        for (name, sql) in migrations {
            let applied = sqlx::query("SELECT 1 FROM _migrations WHERE name = $1")
                .bind(*name)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            if applied.is_some() {
                continue;
            }

            // Apply every statement and record the migration in one transaction,
            // so a crash mid-migration rolls back cleanly and the migration is
            // retried as a whole. Migration 004 drops and rebuilds `postings`;
            // without the transaction a partial apply would leave the schema in a
            // state the migration cannot be re-run against. Both SQLite and
            // PostgreSQL support transactional DDL.
            let mut tx = self
                .pool
                .begin()
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;

            for statement in sql.split(';') {
                let trimmed = statement.trim();
                if !trimmed.is_empty() {
                    sqlx::query(trimmed)
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| StoreError::Internal(e.to_string()))?;
                }
            }

            sqlx::query("INSERT INTO _migrations (name) VALUES ($1)")
                .bind(*name)
                .execute(&mut *tx)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;

            tx.commit()
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Serialization helpers
// ---------------------------------------------------------------------------

/// Serialize a value to a JSON string. Payload columns store JSON as `TEXT` so
/// the database is directly readable for auditing; the ledger never queries
/// into the JSON, so no binary or indexed representation is needed.
fn serialize_json<T: serde::Serialize>(val: &T) -> Result<String, StoreError> {
    serde_json::to_string(val).map_err(|e| StoreError::Internal(format!("json serialization: {e}")))
}

fn deserialize_json<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, StoreError> {
    serde_json::from_str(s).map_err(|e| StoreError::Internal(format!("bad json: {e}")))
}

/// Lower-case hex encoding. Binary identifiers (content-addressed hashes) and
/// opaque saga bytes are stored as hex `TEXT` so a row is legible in any SQL
/// client and matches the hex form used in logs and `Debug` output.
fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn from_hex(s: &str) -> Result<Vec<u8>, StoreError> {
    if s.len() % 2 != 0 {
        return Err(StoreError::Internal(format!("odd-length hex: {s:?}")));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|e| StoreError::Internal(format!("bad hex: {e}")))
        })
        .collect()
}

fn envelope_id_to_hex(id: &EnvelopeId) -> String {
    to_hex(&id.0)
}

fn envelope_id_from_hex(s: &str) -> Result<EnvelopeId, StoreError> {
    let bytes = from_hex(s)?;
    let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        StoreError::Internal(format!("expected 32-byte id, got {} bytes", bytes.len()))
    })?;
    Ok(EnvelopeId(arr))
}

fn row_to_account(row: &sqlx::any::AnyRow) -> Result<Account, StoreError> {
    let id: i64 = row
        .try_get("id")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let subaccount: i64 = row
        .try_get("subaccount")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let version: i64 = row
        .try_get("version")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let flags_bits: i32 = row
        .try_get("flags")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let book: i64 = row
        .try_get("book")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let metadata_json: String = row
        .try_get("metadata")
        .map_err(|e| StoreError::Internal(e.to_string()))?;

    Ok(Account {
        id: AccountId::with_sub(id, subaccount),
        version: version as u64,
        flags: AccountFlags::from_bits_truncate(flags_bits as u32),
        book: BookId::new(book),
        metadata: deserialize_json(&metadata_json)?,
    })
}

fn row_to_posting(row: &sqlx::any::AnyRow) -> Result<Posting, StoreError> {
    let transfer_id: String = row
        .try_get("transfer_id")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let idx: i16 = row
        .try_get("idx")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let owner: i64 = row
        .try_get("owner")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let subaccount: i64 = row
        .try_get("subaccount")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let asset: i32 = row
        .try_get("asset")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let value: String = row
        .try_get("value")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let value = Cent::from_str(&value).map_err(|e| StoreError::Internal(e.to_string()))?;

    Ok(Posting {
        id: PostingId {
            transfer: envelope_id_from_hex(&transfer_id)?,
            index: idx as u16,
        },
        owner: AccountId::with_sub(owner, subaccount),
        asset: AssetId::new(asset as u32),
        value,
    })
}

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
// AccountStore
// ---------------------------------------------------------------------------

#[async_trait]
impl AccountStore for SqlStore {
    async fn get_account(&self, id: &AccountId) -> Result<Account, StoreError> {
        // The head points at the current version, so this is a single indexed
        // lookup into the immutable history — no scan of the version chain.
        let row = sqlx::query(
            "SELECT a.* FROM accounts a \
             JOIN account_head h \
             ON h.id = a.id AND h.subaccount = a.subaccount AND h.version = a.version \
             WHERE h.id = $1 AND h.subaccount = $2",
        )
        .bind(id.id)
        .bind(id.sub)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| StoreError::Internal(e.to_string()))?
        .ok_or_else(|| StoreError::NotFound(format!("account {id:?}")))?;
        row_to_account(&row)
    }

    async fn get_accounts(&self, ids: &[AccountId]) -> Result<Vec<Account>, StoreError> {
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            result.push(self.get_account(id).await?);
        }
        Ok(result)
    }

    async fn create_account(&self, account: Account) -> Result<(), StoreError> {
        // Pessimistic locking: inside one transaction, lock the account's head
        // row with `SELECT ... FOR UPDATE` so a concurrent creator waits. The
        // head is the single row per account; its `ON CONFLICT (id, subaccount)
        // DO NOTHING` insert is the portable backstop that decides the winner
        // (SQLite has no `FOR UPDATE`, and it turns a concurrent double-create
        // into a clean affected-row count instead of a unique violation).
        let lock = self.lock_clause().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        let existing = sqlx::query(&format!(
            "SELECT 1 FROM account_head WHERE id = $1 AND subaccount = $2 LIMIT 1{lock}"
        ))
        .bind(account.id.id)
        .bind(account.id.sub)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| StoreError::Internal(e.to_string()))?;
        if existing.is_some() {
            return Err(StoreError::AlreadyExists(format!(
                "account {:?}",
                account.id
            )));
        }

        // Append the immutable first version, then point the head at it.
        sqlx::query(
            "INSERT INTO accounts (id, subaccount, version, flags, book, metadata) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id, subaccount, version) DO NOTHING"
        )
            .bind(account.id.id)
            .bind(account.id.sub)
            .bind(account.version as i64)
            .bind(account.flags.bits() as i32)
            .bind(account.book.0)
            .bind(serialize_json(&account.metadata)?)
            .execute(&mut *tx)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        let res = sqlx::query(
            "INSERT INTO account_head (id, subaccount, version) VALUES ($1, $2, $3) ON CONFLICT (id, subaccount) DO NOTHING",
        )
        .bind(account.id.id)
        .bind(account.id.sub)
        .bind(account.version as i64)
        .execute(&mut *tx)
        .await
        .map_err(|e| StoreError::Internal(e.to_string()))?;
        if res.rows_affected() == 0 {
            return Err(StoreError::AlreadyExists(format!(
                "account {:?}",
                account.id
            )));
        }

        tx.commit()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn append_account_version(&self, account: Account) -> Result<(), StoreError> {
        // Pessimistic locking: inside one transaction, lock the account's head
        // row with `SELECT ... FOR UPDATE` so a concurrent appender waits here
        // until we commit, then check the version, append the new immutable row,
        // and move the head. `ON CONFLICT` is the portable backstop (SQLite has
        // no `FOR UPDATE`, and it covers the append phantom-insert a row lock
        // does not). The head is maintained by delete + insert, never `UPDATE`,
        // so the write path issues only inserts and deletes.
        let lock = self.lock_clause().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        let current = sqlx::query(&format!(
            "SELECT version FROM account_head WHERE id = $1 AND subaccount = $2{lock}"
        ))
        .bind(account.id.id)
        .bind(account.id.sub)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| StoreError::Internal(e.to_string()))?
        .ok_or_else(|| StoreError::NotFound(format!("account {:?}", account.id)))?;

        let current_version: i64 = current
            .try_get("version")
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let expected = current_version
            .checked_add(1)
            .ok_or_else(|| StoreError::Internal("account version overflow".to_string()))?;

        if account.version as i64 != expected {
            return Err(StoreError::VersionConflict {
                account: account.id,
                expected: expected as u64,
                actual: account.version,
            });
        }

        let res = sqlx::query(
            "INSERT INTO accounts (id, subaccount, version, flags, book, metadata) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id, subaccount, version) DO NOTHING"
        )
            .bind(account.id.id)
            .bind(account.id.sub)
            .bind(account.version as i64)
            .bind(account.flags.bits() as i32)
            .bind(account.book.0)
            .bind(serialize_json(&account.metadata)?)
            .execute(&mut *tx)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        if res.rows_affected() == 0 {
            return Err(StoreError::VersionConflict {
                account: account.id,
                expected: expected as u64,
                actual: account.version,
            });
        }

        // Move the head to the new version (delete + insert, never update).
        sqlx::query("DELETE FROM account_head WHERE id = $1 AND subaccount = $2")
            .bind(account.id.id)
            .bind(account.id.sub)
            .execute(&mut *tx)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        sqlx::query("INSERT INTO account_head (id, subaccount, version) VALUES ($1, $2, $3)")
            .bind(account.id.id)
            .bind(account.id.sub)
            .bind(account.version as i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn get_account_history(&self, id: &AccountId) -> Result<Vec<Account>, StoreError> {
        let rows = sqlx::query(
            "SELECT * FROM accounts WHERE id = $1 AND subaccount = $2 ORDER BY version ASC",
        )
        .bind(id.id)
        .bind(id.sub)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Internal(e.to_string()))?;
        if rows.is_empty() {
            return Err(StoreError::NotFound(format!("account {id:?}")));
        }
        rows.iter().map(row_to_account).collect()
    }

    async fn list_accounts(&self) -> Result<Vec<Account>, StoreError> {
        // One row per account via the head; no read-all-versions + dedup.
        let rows = sqlx::query(
            "SELECT a.* FROM accounts a \
             JOIN account_head h \
             ON h.id = a.id AND h.subaccount = a.subaccount AND h.version = a.version",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Internal(e.to_string()))?;
        rows.iter().map(row_to_account).collect()
    }
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
        let row_key = |row: &sqlx::any::AnyRow| -> Result<(String, i16), StoreError> {
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

// ---------------------------------------------------------------------------
// TransferStore
// ---------------------------------------------------------------------------

#[async_trait]
impl TransferStore for SqlStore {
    async fn get_transfer(&self, id: &EnvelopeId) -> Result<Option<EnvelopeRecord>, StoreError> {
        let row = sqlx::query("SELECT transfer, receipt, created_at FROM transfers WHERE id = $1")
            .bind(envelope_id_to_hex(id))
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        match row {
            None => Ok(None),
            Some(row) => {
                let transfer_json: String = row
                    .try_get("transfer")
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let receipt_json: String = row
                    .try_get("receipt")
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let created_at: i64 = row
                    .try_get("created_at")
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                Ok(Some(EnvelopeRecord {
                    envelope: deserialize_json(&transfer_json)?,
                    receipt: deserialize_json(&receipt_json)?,
                    created_at,
                }))
            }
        }
    }

    async fn store_transfer(
        &self,
        record: EnvelopeRecord,
        involved: &[AccountId],
    ) -> Result<u64, StoreError> {
        let tid = record.receipt.transfer_id;
        let tid_hex = envelope_id_to_hex(&tid);
        let transfer_json = serialize_json(&record.envelope)?;
        let receipt_json = serialize_json(&record.receipt)?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        let res = sqlx::query("INSERT INTO transfers (id, transfer, receipt, created_at, book) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (id) DO NOTHING")
            .bind(&tid_hex)
            .bind(&transfer_json)
            .bind(&receipt_json)
            .bind(record.created_at)
            .bind(record.envelope.book().0)
            .execute(&mut *tx)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let inserted = res.rows_affected();

        // Index every involved account (caller supplies the set; storage does no
        // computation). Idempotent so a replay is harmless.
        for account in involved {
            sqlx::query("INSERT INTO transfer_accounts (transfer_id, account_id, subaccount) VALUES ($1, $2, $3) ON CONFLICT (transfer_id, account_id, subaccount) DO NOTHING")
                .bind(&tid_hex)
                .bind(account.id)
                .bind(account.sub)
                .execute(&mut *tx)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
        }

        tx.commit()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(inserted)
    }

    async fn get_transfers_for_account(
        &self,
        id: i64,
        sub: Option<i64>,
    ) -> Result<Vec<EnvelopeRecord>, StoreError> {
        // `sub == None` spans every subaccount of `id`; `Some(s)` restricts to
        // one. The subaccount is matched only for equality.
        let mut sql = String::from(
            "SELECT t.id, t.transfer, t.receipt, t.created_at FROM transfers t INNER JOIN transfer_accounts ta ON t.id = ta.transfer_id WHERE ta.account_id = $1",
        );
        if sub.is_some() {
            sql.push_str(" AND ta.subaccount = $2");
        }
        sql.push_str(" ORDER BY t.created_at");

        let mut q = sqlx::query(&sql).bind(id);
        if let Some(s) = sub {
            q = q.bind(s);
        }
        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        let mut result = Vec::with_capacity(rows.len());
        for row in &rows {
            let transfer_json: String = row
                .try_get("transfer")
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            let receipt_json: String = row
                .try_get("receipt")
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            let created_at: i64 = row
                .try_get("created_at")
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            result.push(EnvelopeRecord {
                envelope: deserialize_json(&transfer_json)?,
                receipt: deserialize_json(&receipt_json)?,
                created_at,
            });
        }
        Ok(result)
    }

    async fn query_transfers(
        &self,
        query: &TransferQuery,
    ) -> Result<Page<EnvelopeRecord>, StoreError> {
        // Load base records, using the account join when available.
        let base_records = if let Some(account) = query.account {
            self.get_transfers_for_account(account, query.sub).await?
        } else {
            let rows = sqlx::query(
                "SELECT transfer, receipt, created_at FROM transfers ORDER BY created_at",
            )
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

            let mut records = Vec::with_capacity(rows.len());
            for row in &rows {
                let transfer_json: String = row
                    .try_get("transfer")
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let receipt_json: String = row
                    .try_get("receipt")
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let created_at: i64 = row
                    .try_get("created_at")
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                records.push(EnvelopeRecord {
                    envelope: deserialize_json(&transfer_json)?,
                    receipt: deserialize_json(&receipt_json)?,
                    created_at,
                });
            }
            records
        };

        // The account/subaccount narrowing happened in the load above; the
        // shared filter covers the time-window and book predicates, then the
        // shared page cut applies `offset`/`limit`.
        Ok(paginate(
            filter_transfers(base_records, query),
            query.offset,
            query.limit,
        ))
    }
}

// ---------------------------------------------------------------------------
// SagaStore
// ---------------------------------------------------------------------------

#[async_trait]
impl SagaStore for SqlStore {
    async fn save_saga(&self, id: &i64, data: Vec<u8>) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO sagas (id, data) VALUES ($1, $2) \
             ON CONFLICT (id) DO UPDATE SET data = EXCLUDED.data",
        )
        .bind(*id)
        .bind(to_hex(&data))
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn list_pending_sagas(&self) -> Result<Vec<(i64, Vec<u8>)>, StoreError> {
        let rows = sqlx::query("SELECT id, data FROM sagas")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let mut result = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: i64 = row
                .try_get("id")
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            let data_hex: String = row
                .try_get("data")
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            result.push((id, from_hex(&data_hex)?));
        }
        Ok(result)
    }

    async fn delete_saga(&self, id: &i64) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM sagas WHERE id = $1")
            .bind(*id)
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// EventStore
// ---------------------------------------------------------------------------

#[async_trait]
impl EventStore for SqlStore {
    async fn append_event(&self, event: &LedgerEvent) -> Result<u64, StoreError> {
        let kind_str =
            serde_json::to_string(&event.kind).map_err(|e| StoreError::Internal(e.to_string()))?;
        let data = serialize_json(event)?;
        let seq = self.autoid.next() as u64;

        // Idempotent on the dedup key: a replayed transfer event conflicts on
        // `dedup_key` and returns the existing seq instead of a duplicate row.
        match kuatia_storage::events::event_dedup_key(&event.kind) {
            Some(eid) => {
                let dedup_hex = envelope_id_to_hex(&eid);
                let res = sqlx::query("INSERT INTO events (seq, timestamp, kind, data, dedup_key) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (dedup_key) DO NOTHING")
                    .bind(seq as i64)
                    .bind(event.timestamp)
                    .bind(&kind_str)
                    .bind(&data)
                    .bind(&dedup_hex)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                if res.rows_affected() == 0 {
                    let row = sqlx::query("SELECT seq FROM events WHERE dedup_key = $1")
                        .bind(&dedup_hex)
                        .fetch_one(&self.pool)
                        .await
                        .map_err(|e| StoreError::Internal(e.to_string()))?;
                    let existing: i64 = row
                        .try_get("seq")
                        .map_err(|e| StoreError::Internal(e.to_string()))?;
                    return Ok(existing as u64);
                }
                Ok(seq)
            }
            None => {
                sqlx::query(
                    "INSERT INTO events (seq, timestamp, kind, data) VALUES ($1, $2, $3, $4)",
                )
                .bind(seq as i64)
                .bind(event.timestamp)
                .bind(&kind_str)
                .bind(&data)
                .execute(&self.pool)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
                Ok(seq)
            }
        }
    }

    async fn get_events_since(
        &self,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<LedgerEvent>, StoreError> {
        let rows = sqlx::query("SELECT seq, data FROM events WHERE seq > $1 ORDER BY seq LIMIT $2")
            .bind(after_seq as i64)
            .bind(limit as i32)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        let mut events = Vec::with_capacity(rows.len());
        for row in &rows {
            let seq: i64 = row
                .try_get("seq")
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            let data_json: String = row
                .try_get("data")
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            let mut event: LedgerEvent = deserialize_json(&data_json)?;
            event.seq = seq as u64;
            events.push(event);
        }
        Ok(events)
    }
}

// ---------------------------------------------------------------------------
// BookStore
// ---------------------------------------------------------------------------

#[async_trait]
impl BookStore for SqlStore {
    async fn create_book(&self, book: Book) -> Result<(), StoreError> {
        // Pessimistic locking, same shape as create_account: lock any existing
        // book row with `SELECT ... FOR UPDATE` inside the transaction, then
        // insert with `ON CONFLICT DO NOTHING` as the portable backstop.
        let lock = self.lock_clause().await?;
        let data = serialize_json(&book)?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        let existing = sqlx::query(&format!("SELECT 1 FROM books WHERE id = $1 LIMIT 1{lock}"))
            .bind(book.id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        if existing.is_some() {
            return Err(StoreError::AlreadyExists(format!("book {:?}", book.id)));
        }

        let res = sqlx::query(
            "INSERT INTO books (id, name, data) VALUES ($1, $2, $3) ON CONFLICT (id) DO NOTHING",
        )
        .bind(book.id.0)
        .bind(&book.name)
        .bind(&data)
        .execute(&mut *tx)
        .await
        .map_err(|e| StoreError::Internal(e.to_string()))?;
        if res.rows_affected() == 0 {
            return Err(StoreError::AlreadyExists(format!("book {:?}", book.id)));
        }

        tx.commit()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn get_book(&self, id: &BookId) -> Result<Book, StoreError> {
        let row = sqlx::query("SELECT data FROM books WHERE id = $1")
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?
            .ok_or_else(|| StoreError::NotFound(format!("book {id:?}")))?;
        let data: String = row
            .try_get("data")
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        deserialize_json(&data)
    }

    async fn list_books(&self) -> Result<Vec<Book>, StoreError> {
        let rows = sqlx::query("SELECT data FROM books")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        rows.iter()
            .map(|row| {
                let data: String = row
                    .try_get("data")
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                deserialize_json(&data)
            })
            .collect()
    }
}
