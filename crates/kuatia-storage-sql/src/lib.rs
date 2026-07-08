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

use std::str::FromStr;
use std::sync::atomic::{AtomicU8, Ordering};

use async_trait::async_trait;
use sqlx::{Any, Pool, Row};

use kuatia_storage::error::StoreError;
use kuatia_storage::events::{EventStore, LedgerEvent};
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

            for statement in sql.split(';') {
                let trimmed = statement.trim();
                if !trimmed.is_empty() {
                    sqlx::query(trimmed)
                        .execute(&self.pool)
                        .await
                        .map_err(|e| StoreError::Internal(e.to_string()))?;
                }
            }

            sqlx::query("INSERT INTO _migrations (name) VALUES ($1)")
                .bind(*name)
                .execute(&self.pool)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Serialization helpers
// ---------------------------------------------------------------------------

fn serialize_policy(policy: &AccountPolicy) -> Result<String, StoreError> {
    serde_json::to_string(policy)
        .map_err(|e| StoreError::Internal(format!("policy serialization: {e}")))
}

fn deserialize_policy(s: &str) -> Result<AccountPolicy, StoreError> {
    serde_json::from_str(s).map_err(|e| StoreError::Internal(format!("bad policy: {e}")))
}

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
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
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

fn status_to_i16(s: PostingStatus) -> i16 {
    match s {
        PostingStatus::Active => 0,
        PostingStatus::PendingInactive => 1,
        PostingStatus::Inactive => 2,
    }
}

fn status_from_i16(v: i16) -> Result<PostingStatus, StoreError> {
    match v {
        0 => Ok(PostingStatus::Active),
        1 => Ok(PostingStatus::PendingInactive),
        2 => Ok(PostingStatus::Inactive),
        _ => Err(StoreError::Internal(format!("bad posting status: {v}"))),
    }
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
    let policy_str: String = row
        .try_get("policy")
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
        policy: deserialize_policy(&policy_str)?,
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
    let status: i16 = row
        .try_get("status")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let reservation: Option<i64> = row
        .try_get("reservation")
        .map_err(|e| StoreError::Internal(e.to_string()))?;

    Ok(Posting {
        id: PostingId {
            transfer: envelope_id_from_hex(&transfer_id)?,
            index: idx as u16,
        },
        owner: AccountId::with_sub(owner, subaccount),
        asset: AssetId::new(asset as u32),
        value,
        status: status_from_i16(status)?,
        reservation: reservation.map(ReservationId::new),
    })
}

// ---------------------------------------------------------------------------
// AccountStore
// ---------------------------------------------------------------------------

#[async_trait]
impl AccountStore for SqlStore {
    async fn get_account(&self, id: &AccountId) -> Result<Account, StoreError> {
        let row = sqlx::query(
            "SELECT * FROM accounts WHERE id = $1 AND subaccount = $2 ORDER BY version DESC LIMIT 1",
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
        // Pessimistic locking: inside one transaction, lock any existing row for
        // this account with `SELECT ... FOR UPDATE` so a concurrent creator
        // waits, then insert. `ON CONFLICT DO NOTHING` is the portable backstop
        // (SQLite has no `FOR UPDATE`, and it turns a concurrent double-insert
        // into a clean affected-row count instead of a unique violation).
        let lock = self.lock_clause().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        let existing = sqlx::query(&format!(
            "SELECT 1 FROM accounts WHERE id = $1 AND subaccount = $2 LIMIT 1{lock}"
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

        let res = sqlx::query(
            "INSERT INTO accounts (id, subaccount, version, policy, flags, book, metadata) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (id, subaccount, version) DO NOTHING"
        )
            .bind(account.id.id)
            .bind(account.id.sub)
            .bind(account.version as i64)
            .bind(serialize_policy(&account.policy)?)
            .bind(account.flags.bits() as i32)
            .bind(account.book.0)
            .bind(serialize_json(&account.metadata)?)
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
        // Pessimistic locking: inside one transaction, lock the account's latest
        // version row with `SELECT ... FOR UPDATE` so a concurrent appender waits
        // here until we commit, then check the version and insert. `ON CONFLICT`
        // is the portable backstop (SQLite has no `FOR UPDATE`, and it covers the
        // append phantom-insert a row lock does not).
        let lock = self.lock_clause().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        let current = sqlx::query(&format!(
            "SELECT version FROM accounts WHERE id = $1 AND subaccount = $2 ORDER BY version DESC LIMIT 1{lock}"
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
            "INSERT INTO accounts (id, subaccount, version, policy, flags, book, metadata) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (id, subaccount, version) DO NOTHING"
        )
            .bind(account.id.id)
            .bind(account.id.sub)
            .bind(account.version as i64)
            .bind(serialize_policy(&account.policy)?)
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
        let rows = sqlx::query("SELECT * FROM accounts ORDER BY id, subaccount, version DESC")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let mut accounts: Vec<Account> =
            rows.iter().map(row_to_account).collect::<Result<_, _>>()?;
        accounts.dedup_by_key(|a| a.id);
        Ok(accounts)
    }
}

// ---------------------------------------------------------------------------
// PostingStore
// ---------------------------------------------------------------------------

#[async_trait]
impl PostingStore for SqlStore {
    async fn get_postings(&self, ids: &[PostingId]) -> Result<Vec<Posting>, StoreError> {
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            let row = sqlx::query("SELECT * FROM postings WHERE transfer_id = $1 AND idx = $2")
                .bind(envelope_id_to_hex(&id.transfer))
                .bind(id.index as i16)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?
                .ok_or_else(|| StoreError::NotFound(format!("posting {id:?}")))?;
            result.push(row_to_posting(&row)?);
        }
        Ok(result)
    }

    async fn get_postings_by_account(
        &self,
        id: i64,
        sub: Option<i64>,
        asset: Option<&AssetId>,
        status: Option<PostingStatus>,
    ) -> Result<Vec<Posting>, StoreError> {
        // Build the predicate dynamically: `sub == None` spans every subaccount
        // of `id`, `Some(s)` restricts to one. The subaccount is compared only
        // for equality, never as a magnitude.
        let mut sql = String::from("SELECT * FROM postings WHERE owner = $1");
        let mut placeholder = 2u32;
        if sub.is_some() {
            sql.push_str(&format!(" AND subaccount = ${placeholder}"));
            placeholder += 1;
        }
        if asset.is_some() {
            sql.push_str(&format!(" AND asset = ${placeholder}"));
            placeholder += 1;
        }
        if status.is_some() {
            sql.push_str(&format!(" AND status = ${placeholder}"));
        }

        let mut q = sqlx::query(&sql).bind(id);
        if let Some(s) = sub {
            q = q.bind(s);
        }
        if let Some(a) = asset {
            q = q.bind(a.0 as i32);
        }
        if let Some(s) = status {
            q = q.bind(status_to_i16(s));
        }

        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        rows.iter().map(row_to_posting).collect()
    }

    async fn query_postings(&self, query: &PostingQuery) -> Result<Page<Posting>, StoreError> {
        let (where_clause, count_clause) = {
            let mut w = String::from("WHERE owner = $1");
            let mut idx = 2u32;
            if query.sub.is_some() {
                w.push_str(&format!(" AND subaccount = ${idx}"));
                idx += 1;
            }
            if query.asset.is_some() {
                w.push_str(&format!(" AND asset = ${idx}"));
                idx += 1;
            }
            if query.status.is_some() {
                w.push_str(&format!(" AND status = ${idx}"));
            }
            let c = format!("SELECT COUNT(*) as cnt FROM postings {w}");
            let limit = query.limit.unwrap_or(u32::MAX);
            let offset = query.offset.unwrap_or(0);
            w.push_str(&format!(" LIMIT {limit} OFFSET {offset}"));
            (format!("SELECT * FROM postings {w}"), c)
        };

        // Build count query
        let mut count_q = sqlx::query(&count_clause).bind(query.account);
        if let Some(s) = query.sub {
            count_q = count_q.bind(s);
        }
        if let Some(ref a) = query.asset {
            count_q = count_q.bind(a.0 as i32);
        }
        if let Some(s) = query.status {
            count_q = count_q.bind(status_to_i16(s));
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
        if let Some(s) = query.status {
            data_q = data_q.bind(status_to_i16(s));
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
        // Dumb instruction: each id flips Active → PendingInactive (the status
        // precondition is in the WHERE so it is atomic). Return the count of rows
        // changed; the caller decides what a short count means.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let mut reserved: u64 = 0;
        for id in ids {
            let res = sqlx::query(
                "UPDATE postings SET status = $1, reservation = $2 WHERE transfer_id = $3 AND idx = $4 AND status = $5",
            )
            .bind(status_to_i16(PostingStatus::PendingInactive))
            .bind(reservation.0)
            .bind(envelope_id_to_hex(&id.transfer))
            .bind(id.index as i16)
            .bind(status_to_i16(PostingStatus::Active))
            .execute(&mut *tx)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
            reserved += res.rows_affected();
        }

        tx.commit()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(reserved)
    }

    async fn release_postings(
        &self,
        ids: &[PostingId],
        reservation: ReservationId,
    ) -> Result<u64, StoreError> {
        // Dumb instruction: each id reserved by `reservation` flips
        // PendingInactive → Active. Return the count released; an already-Active
        // or differently-owned posting simply does not count.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let mut released: u64 = 0;
        for id in ids {
            let res = sqlx::query("UPDATE postings SET status = $1, reservation = NULL WHERE transfer_id = $2 AND idx = $3 AND status = $4 AND reservation = $5")
                .bind(status_to_i16(PostingStatus::Active))
                .bind(envelope_id_to_hex(&id.transfer))
                .bind(id.index as i16)
                .bind(status_to_i16(PostingStatus::PendingInactive))
                .bind(reservation.0)
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
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let mut changed: u64 = 0;
        for id in ids {
            // The precondition is the instruction; the count is the result. The
            // caller decides what a short count means.
            let res = match reservation {
                None => {
                    sqlx::query("UPDATE postings SET status = $1, reservation = NULL WHERE transfer_id = $2 AND idx = $3 AND status = $4")
                        .bind(status_to_i16(PostingStatus::Inactive))
                        .bind(envelope_id_to_hex(&id.transfer))
                        .bind(id.index as i16)
                        .bind(status_to_i16(PostingStatus::Active))
                        .execute(&mut *tx)
                        .await
                }
                Some(rid) => {
                    sqlx::query("UPDATE postings SET status = $1, reservation = NULL WHERE transfer_id = $2 AND idx = $3 AND status = $4 AND reservation = $5")
                        .bind(status_to_i16(PostingStatus::Inactive))
                        .bind(envelope_id_to_hex(&id.transfer))
                        .bind(id.index as i16)
                        .bind(status_to_i16(PostingStatus::PendingInactive))
                        .bind(rid.0)
                        .execute(&mut *tx)
                        .await
                }
            }
            .map_err(|e| StoreError::Internal(e.to_string()))?;
            changed += res.rows_affected();
        }
        tx.commit()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(changed)
    }

    async fn insert_postings(&self, postings: &[Posting]) -> Result<u64, StoreError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let mut inserted: u64 = 0;
        for posting in postings {
            let res = sqlx::query(
                "INSERT INTO postings (transfer_id, idx, owner, subaccount, asset, value, status) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (transfer_id, idx) DO NOTHING"
            )
                .bind(envelope_id_to_hex(&posting.id.transfer))
                .bind(posting.id.index as i16)
                .bind(posting.owner.id)
                .bind(posting.owner.sub)
                .bind(posting.asset.0 as i32)
                .bind(posting.value.to_string())
                .bind(status_to_i16(posting.status))
                .execute(&mut *tx)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            inserted += res.rows_affected();
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

        // Filter in memory for remaining conditions.
        let filtered: Vec<EnvelopeRecord> = base_records
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
            .collect();

        let total = filtered.len() as u64;
        let offset = query.offset.unwrap_or(0) as usize;
        let limit = query.limit.unwrap_or(u32::MAX) as usize;
        let items = filtered.into_iter().skip(offset).take(limit).collect();

        Ok(Page { items, total })
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
