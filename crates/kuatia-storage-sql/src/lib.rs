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

use std::collections::HashSet;

use async_trait::async_trait;
use sqlx::{Any, Pool, Row};

use kuatia_storage::error::StoreError;
use kuatia_storage::events::{EventStore, LedgerEvent};
use kuatia_storage::store::*;
use kuatia_types::*;

/// SQL-backed [`Store`] implementation.
pub struct SqlStore {
    pool: Pool<Any>,
    autoid: kuatia_types::autoid::AutoId,
}

impl SqlStore {
    /// Create a new SQL store wrapping an existing connection pool.
    pub fn new(pool: Pool<Any>) -> Self {
        Self {
            pool,
            autoid: kuatia_types::autoid::AutoId::new(),
        }
    }

    /// Run database migrations. Idempotent: a `_migrations` ledger records what
    /// has been applied, so re-running is a no-op. The DDL is selected per
    /// backend (SQLite uses `BLOB`, PostgreSQL uses `BYTEA`).
    pub async fn migrate(&self) -> Result<(), StoreError> {
        // Detect the backend. `sqlite_version()` exists only on SQLite.
        let is_sqlite = sqlx::query("SELECT sqlite_version()")
            .fetch_optional(&self.pool)
            .await
            .is_ok();

        sqlx::query("CREATE TABLE IF NOT EXISTS _migrations (name TEXT PRIMARY KEY)")
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        let migrations: &[(&str, &str)] = if is_sqlite {
            &[("001_init", include_str!("migrations/sqlite/001_init.sql"))]
        } else {
            &[("001_init", include_str!("migrations/postgres/001_init.sql"))]
        };

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

fn serialize_blob<T: serde::Serialize>(val: &T) -> Result<Vec<u8>, StoreError> {
    serde_json::to_vec(val).map_err(|e| StoreError::Internal(format!("blob serialization: {e}")))
}

fn deserialize_blob<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, StoreError> {
    serde_json::from_slice(bytes).map_err(|e| StoreError::Internal(format!("bad blob: {e}")))
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
    let user_data_bytes: Vec<u8> = row
        .try_get("user_data")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let metadata_bytes: Vec<u8> = row
        .try_get("metadata")
        .map_err(|e| StoreError::Internal(e.to_string()))?;

    Ok(Account {
        id: AccountId::new(id),
        version: version as u64,
        policy: deserialize_policy(&policy_str)?,
        flags: AccountFlags::from_bits_truncate(flags_bits as u32),
        book: BookId::new(book),
        user_data: deserialize_blob(&user_data_bytes)?,
        metadata: deserialize_blob(&metadata_bytes)?,
    })
}

fn row_to_posting(row: &sqlx::any::AnyRow) -> Result<Posting, StoreError> {
    let transfer_id: Vec<u8> = row
        .try_get("transfer_id")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let idx: i16 = row
        .try_get("idx")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let owner: i64 = row
        .try_get("owner")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let asset: i32 = row
        .try_get("asset")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let value: i64 = row
        .try_get("value")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let status: i16 = row
        .try_get("status")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let reservation: Option<i64> = row
        .try_get("reservation")
        .map_err(|e| StoreError::Internal(e.to_string()))?;

    let mut tid = [0u8; 32];
    tid.copy_from_slice(&transfer_id);

    Ok(Posting {
        id: PostingId {
            transfer: EnvelopeId(tid),
            index: idx as u16,
        },
        owner: AccountId::new(owner),
        asset: AssetId::new(asset as u32),
        value: Cent::from(value),
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
        let row = sqlx::query("SELECT * FROM accounts WHERE id = $1 ORDER BY version DESC LIMIT 1")
            .bind(id.0)
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
        let exists = sqlx::query("SELECT 1 FROM accounts WHERE id = $1 LIMIT 1")
            .bind(account.id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        if exists.is_some() {
            return Err(StoreError::AlreadyExists(format!(
                "account {:?}",
                account.id
            )));
        }

        sqlx::query(
            "INSERT INTO accounts (id, version, policy, flags, book, user_data, metadata) VALUES ($1, $2, $3, $4, $5, $6, $7)"
        )
            .bind(account.id.0)
            .bind(account.version as i64)
            .bind(serialize_policy(&account.policy)?)
            .bind(account.flags.bits() as i32)
            .bind(account.book.0)
            .bind(serialize_blob(&account.user_data)?)
            .bind(serialize_blob(&account.metadata)?)
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn append_account_version(&self, account: Account) -> Result<(), StoreError> {
        let current =
            sqlx::query("SELECT version FROM accounts WHERE id = $1 ORDER BY version DESC LIMIT 1")
                .bind(account.id.0)
                .fetch_optional(&self.pool)
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

        sqlx::query(
            "INSERT INTO accounts (id, version, policy, flags, book, user_data, metadata) VALUES ($1, $2, $3, $4, $5, $6, $7)"
        )
            .bind(account.id.0)
            .bind(account.version as i64)
            .bind(serialize_policy(&account.policy)?)
            .bind(account.flags.bits() as i32)
            .bind(account.book.0)
            .bind(serialize_blob(&account.user_data)?)
            .bind(serialize_blob(&account.metadata)?)
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn get_account_history(&self, id: &AccountId) -> Result<Vec<Account>, StoreError> {
        let rows = sqlx::query("SELECT * FROM accounts WHERE id = $1 ORDER BY version ASC")
            .bind(id.0)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        if rows.is_empty() {
            return Err(StoreError::NotFound(format!("account {id:?}")));
        }
        rows.iter().map(row_to_account).collect()
    }

    async fn list_accounts(&self) -> Result<Vec<Account>, StoreError> {
        let rows = sqlx::query("SELECT * FROM accounts ORDER BY id, version DESC")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let mut accounts: Vec<Account> = rows.iter().map(row_to_account).collect::<Result<_, _>>()?;
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
                .bind(id.transfer.0.as_slice())
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
        account: &AccountId,
        asset: Option<&AssetId>,
        status: Option<PostingStatus>,
    ) -> Result<Vec<Posting>, StoreError> {
        let rows = match (asset, status) {
            (Some(a), Some(s)) => {
                sqlx::query(
                    "SELECT * FROM postings WHERE owner = $1 AND asset = $2 AND status = $3",
                )
                .bind(account.0)
                .bind(a.0 as i32)
                .bind(status_to_i16(s))
                .fetch_all(&self.pool)
                .await
            }
            (Some(a), None) => {
                sqlx::query("SELECT * FROM postings WHERE owner = $1 AND asset = $2")
                    .bind(account.0)
                    .bind(a.0 as i32)
                    .fetch_all(&self.pool)
                    .await
            }
            (None, Some(s)) => {
                sqlx::query("SELECT * FROM postings WHERE owner = $1 AND status = $2")
                    .bind(account.0)
                    .bind(status_to_i16(s))
                    .fetch_all(&self.pool)
                    .await
            }
            (None, None) => {
                sqlx::query("SELECT * FROM postings WHERE owner = $1")
                    .bind(account.0)
                    .fetch_all(&self.pool)
                    .await
            }
        }
        .map_err(|e| StoreError::Internal(e.to_string()))?;

        rows.iter().map(row_to_posting).collect()
    }

    async fn query_postings(&self, query: &PostingQuery) -> Result<Page<Posting>, StoreError> {
        let (where_clause, count_clause) = {
            let mut w = String::from("WHERE owner = $1");
            let mut idx = 2u32;
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
        let mut count_q = sqlx::query(&count_clause).bind(query.account.0);
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
        let mut data_q = sqlx::query(&where_clause).bind(query.account.0);
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
    ) -> Result<(), StoreError> {
        // Conditional update: each posting transitions Active → PendingInactive
        // only if it is still Active. The status precondition lives in the WHERE
        // clause so a concurrent transition cannot slip between a check and the
        // write; `rows_affected() == 1` is the authorization. A missing row and a
        // non-Active row are indistinguishable here — both are "not reservable".
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        for id in ids {
            let res = sqlx::query(
                "UPDATE postings SET status = $1, reservation = $2 WHERE transfer_id = $3 AND idx = $4 AND status = $5",
            )
            .bind(status_to_i16(PostingStatus::PendingInactive))
            .bind(reservation.0)
            .bind(id.transfer.0.as_slice())
            .bind(id.index as i16)
            .bind(status_to_i16(PostingStatus::Active))
            .execute(&mut *tx)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
            if res.rows_affected() != 1 {
                return Err(StoreError::PostingNotActive(*id));
            }
        }

        tx.commit()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn release_postings(
        &self,
        ids: &[PostingId],
        reservation: ReservationId,
    ) -> Result<(), StoreError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        // Classify then conditionally release in one pass. Releasing an Active
        // posting is a no-op (matches InMemory); releasing a reserved one must
        // affect exactly one row, otherwise it changed under us concurrently.
        for id in ids {
            let row = sqlx::query(
                "SELECT status, reservation FROM postings WHERE transfer_id = $1 AND idx = $2",
            )
            .bind(id.transfer.0.as_slice())
            .bind(id.index as i16)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?
            .ok_or_else(|| StoreError::NotFound(format!("posting {id:?}")))?;
            let status: i16 = row
                .try_get("status")
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            if status == 2 {
                return Err(StoreError::PostingInactive(*id));
            }
            let owner: Option<i64> = row
                .try_get("reservation")
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            if status == 1 && owner != Some(reservation.0) {
                return Err(StoreError::ReservationMismatch(*id));
            }

            let res = sqlx::query("UPDATE postings SET status = $1, reservation = NULL WHERE transfer_id = $2 AND idx = $3 AND status = $4 AND reservation = $5")
                .bind(status_to_i16(PostingStatus::Active))
                .bind(id.transfer.0.as_slice())
                .bind(id.index as i16)
                .bind(status_to_i16(PostingStatus::PendingInactive))
                .bind(reservation.0)
                .execute(&mut *tx)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            // status == 1 was reserved by us → the row must flip; status == 0
            // (Active) is a no-op with zero rows affected.
            if status == 1 && res.rows_affected() != 1 {
                return Err(StoreError::ReservationMismatch(*id));
            }
        }

        tx.commit()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

}

// ---------------------------------------------------------------------------
// TransferStore
// ---------------------------------------------------------------------------

#[async_trait]
impl TransferStore for SqlStore {
    async fn get_transfer(&self, id: &EnvelopeId) -> Result<Option<EnvelopeRecord>, StoreError> {
        let row = sqlx::query("SELECT transfer, receipt, created_at FROM transfers WHERE id = $1")
            .bind(id.0.as_slice())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        match row {
            None => Ok(None),
            Some(row) => {
                let transfer_bytes: Vec<u8> = row
                    .try_get("transfer")
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let receipt_bytes: Vec<u8> = row
                    .try_get("receipt")
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let created_at: i64 = row
                    .try_get("created_at")
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                Ok(Some(EnvelopeRecord {
                    envelope: deserialize_blob(&transfer_bytes)?,
                    receipt: deserialize_blob(&receipt_bytes)?,
                    created_at,
                }))
            }
        }
    }

    async fn get_transfers_for_account(
        &self,
        account: &AccountId,
    ) -> Result<Vec<EnvelopeRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT t.id, t.transfer, t.receipt, t.created_at FROM transfers t INNER JOIN transfer_accounts ta ON t.id = ta.transfer_id WHERE ta.account_id = $1 ORDER BY t.created_at"
        )
            .bind(account.0)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        let mut result = Vec::with_capacity(rows.len());
        for row in &rows {
            let transfer_bytes: Vec<u8> = row
                .try_get("transfer")
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            let receipt_bytes: Vec<u8> = row
                .try_get("receipt")
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            let created_at: i64 = row
                .try_get("created_at")
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            result.push(EnvelopeRecord {
                envelope: deserialize_blob(&transfer_bytes)?,
                receipt: deserialize_blob(&receipt_bytes)?,
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
        let base_records = if let Some(ref account) = query.account {
            self.get_transfers_for_account(account).await?
        } else {
            let rows = sqlx::query(
                "SELECT transfer, receipt, created_at FROM transfers ORDER BY created_at",
            )
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

            let mut records = Vec::with_capacity(rows.len());
            for row in &rows {
                let transfer_bytes: Vec<u8> = row
                    .try_get("transfer")
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let receipt_bytes: Vec<u8> = row
                    .try_get("receipt")
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                let created_at: i64 = row
                    .try_get("created_at")
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                records.push(EnvelopeRecord {
                    envelope: deserialize_blob(&transfer_bytes)?,
                    receipt: deserialize_blob(&receipt_bytes)?,
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
        .bind(&data)
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
            let data: Vec<u8> = row
                .try_get("data")
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            result.push((id, data));
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
        let data = serialize_blob(event)?;
        let seq = self.autoid.next() as u64;

        sqlx::query("INSERT INTO events (seq, timestamp, kind, data) VALUES ($1, $2, $3, $4)")
            .bind(seq as i64)
            .bind(event.timestamp)
            .bind(&kind_str)
            .bind(&data)
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        Ok(seq)
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
            let data: Vec<u8> = row
                .try_get("data")
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            let mut event: LedgerEvent = deserialize_blob(&data)?;
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
        let exists = sqlx::query("SELECT 1 FROM books WHERE id = $1 LIMIT 1")
            .bind(book.id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        if exists.is_some() {
            return Err(StoreError::AlreadyExists(format!("book {:?}", book.id)));
        }

        let data = serialize_blob(&book)?;
        sqlx::query("INSERT INTO books (id, name, data) VALUES ($1, $2, $3)")
            .bind(book.id.0)
            .bind(&book.name)
            .bind(&data)
            .execute(&self.pool)
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
        let data: Vec<u8> = row
            .try_get("data")
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        deserialize_blob(&data)
    }

    async fn list_books(&self) -> Result<Vec<Book>, StoreError> {
        let rows = sqlx::query("SELECT data FROM books")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        rows.iter()
            .map(|row| {
                let data: Vec<u8> = row
                    .try_get("data")
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                deserialize_blob(&data)
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// CommitStore — the single atomic commit boundary
// ---------------------------------------------------------------------------

#[async_trait]
impl CommitStore for SqlStore {
    async fn commit_transfer(&self, req: CommitRequest<'_>) -> Result<(), StoreError> {
        let tid = req.record.receipt.transfer_id;
        let transfer_bytes = serialize_blob(&req.record.envelope)?;
        let receipt_bytes = serialize_blob(&req.record.receipt)?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        // 1. Idempotency: already committed?
        let exists = sqlx::query("SELECT 1 FROM transfers WHERE id = $1")
            .bind(tid.0.as_slice())
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        if exists.is_some() {
            return Ok(());
        }

        // 2. CAS guards — recompute Σ(non-Inactive) in Rust and compare.
        for (account, asset, expected) in req.cas_guards {
            let rows = sqlx::query(
                "SELECT value FROM postings WHERE owner = $1 AND asset = $2 AND status != $3",
            )
            .bind(account.0)
            .bind(asset.0 as i32)
            .bind(status_to_i16(PostingStatus::Inactive))
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
            let mut sum: i64 = 0;
            for row in &rows {
                let v: i64 = row
                    .try_get("value")
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                sum = sum
                    .checked_add(v)
                    .ok_or_else(|| StoreError::Internal("balance overflow during cas".into()))?;
            }
            if sum != expected.value() {
                return Err(StoreError::Conflict {
                    account: *account,
                    asset: *asset,
                });
            }
        }

        // 2b. Account version guards — re-check each pinned account version in the
        //     same transaction so a concurrent freeze/unfreeze/close (which bumps
        //     version) aborts the commit instead of racing past validation.
        for (account, expected) in req.account_guards {
            let actual: i64 = sqlx::query(
                "SELECT version FROM accounts WHERE id = $1 ORDER BY version DESC LIMIT 1",
            )
            .bind(account.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?
            .ok_or(StoreError::NotFound(format!("account {account:?}")))?
            .try_get("version")
            .map_err(|e| StoreError::Internal(e.to_string()))?;
            if actual as u64 != *expected {
                return Err(StoreError::VersionConflict {
                    account: *account,
                    expected: *expected,
                    actual: actual as u64,
                });
            }
        }

        // 3. Authorize consumed postings and collect their owners.
        let mut account_ids: HashSet<i64> = HashSet::new();
        for pid in req.deactivate {
            let row = sqlx::query(
                "SELECT owner, status, reservation FROM postings WHERE transfer_id = $1 AND idx = $2",
            )
            .bind(pid.transfer.0.as_slice())
            .bind(pid.index as i16)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?
            .ok_or(StoreError::ReservationMismatch(*pid))?;
            let owner: i64 = row
                .try_get("owner")
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            let status: i16 = row
                .try_get("status")
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            let reservation: Option<i64> = row
                .try_get("reservation")
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            match req.reservation {
                None => {
                    if status != status_to_i16(PostingStatus::Active) {
                        return Err(StoreError::ReservationMismatch(*pid));
                    }
                }
                Some(rid) => {
                    if status != status_to_i16(PostingStatus::PendingInactive)
                        || reservation != Some(rid.0)
                    {
                        return Err(StoreError::ReservationMismatch(*pid));
                    }
                }
            }
            account_ids.insert(owner);
        }

        // 4. Deactivate consumed postings. The authorization precondition lives in
        //    the WHERE clause (not just the step-3 read) so two concurrent commits
        //    cannot both deactivate the same posting under READ COMMITTED. The raw
        //    path requires it still Active; the saga path requires it still
        //    PendingInactive owned by this reservation. `rows_affected() == 1` is
        //    the authorization.
        for pid in req.deactivate {
            let res = match req.reservation {
                None => {
                    sqlx::query("UPDATE postings SET status = $1, reservation = NULL WHERE transfer_id = $2 AND idx = $3 AND status = $4")
                        .bind(status_to_i16(PostingStatus::Inactive))
                        .bind(pid.transfer.0.as_slice())
                        .bind(pid.index as i16)
                        .bind(status_to_i16(PostingStatus::Active))
                        .execute(&mut *tx)
                        .await
                }
                Some(rid) => {
                    sqlx::query("UPDATE postings SET status = $1, reservation = NULL WHERE transfer_id = $2 AND idx = $3 AND status = $4 AND reservation = $5")
                        .bind(status_to_i16(PostingStatus::Inactive))
                        .bind(pid.transfer.0.as_slice())
                        .bind(pid.index as i16)
                        .bind(status_to_i16(PostingStatus::PendingInactive))
                        .bind(rid.0)
                        .execute(&mut *tx)
                        .await
                }
            }
            .map_err(|e| StoreError::Internal(e.to_string()))?;
            if res.rows_affected() != 1 {
                return Err(StoreError::ReservationMismatch(*pid));
            }
        }

        // 5. Insert created postings (Active, unreserved → reservation defaults NULL).
        for posting in req.create {
            sqlx::query(
                "INSERT INTO postings (transfer_id, idx, owner, asset, value, status) VALUES ($1, $2, $3, $4, $5, $6)"
            )
                .bind(posting.id.transfer.0.as_slice())
                .bind(posting.id.index as i16)
                .bind(posting.owner.0)
                .bind(posting.asset.0 as i32)
                .bind(posting.value.value())
                .bind(status_to_i16(posting.status))
                .execute(&mut *tx)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            account_ids.insert(posting.owner.0);
        }

        // 6. Persist the transfer record.
        sqlx::query("INSERT INTO transfers (id, transfer, receipt, created_at, book) VALUES ($1, $2, $3, $4, $5)")
            .bind(tid.0.as_slice())
            .bind(&transfer_bytes)
            .bind(&receipt_bytes)
            .bind(req.record.created_at)
            .bind(req.record.envelope.book().0)
            .execute(&mut *tx)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        // 7. Index transfer_accounts from BOTH created and consumed owners.
        for aid in &account_ids {
            sqlx::query("INSERT INTO transfer_accounts (transfer_id, account_id) VALUES ($1, $2)")
                .bind(tid.0.as_slice())
                .bind(*aid)
                .execute(&mut *tx)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
        }

        // 8. Append events within the same transaction.
        for event in req.events {
            let kind_str = serde_json::to_string(&event.kind)
                .map_err(|e| StoreError::Internal(e.to_string()))?;
            let data = serialize_blob(event)?;
            let seq = self.autoid.next() as u64;
            sqlx::query("INSERT INTO events (seq, timestamp, kind, data) VALUES ($1, $2, $3, $4)")
                .bind(seq as i64)
                .bind(event.timestamp)
                .bind(&kind_str)
                .bind(&data)
                .execute(&mut *tx)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
        }

        tx.commit()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }
}
