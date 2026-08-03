//! [`AccountStore`]: append-only account versions with a head pointer.

use async_trait::async_trait;
use sqlx::Row;

use kuatia_storage::error::StoreError;
use kuatia_storage::store::*;
use kuatia_types::*;

use crate::SqlStore;
use crate::row::{row_to_account, serialize_json};

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

    async fn create_account(&self, account: Account) -> Result<u64, StoreError> {
        // Pessimistic locking: inside one transaction, lock the account's head
        // row with `SELECT ... FOR UPDATE` so a concurrent creator waits. The
        // head is the single row per account; its `ON CONFLICT (id, subaccount)
        // DO NOTHING` insert is the portable backstop that decides the winner
        // (SQLite has no `FOR UPDATE`, and it turns a concurrent double-create
        // into a clean affected-row count instead of a unique violation).
        let lock = self.dialect.lock_clause();
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
            return Ok(0);
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
            return Ok(0);
        }

        tx.commit()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(1)
    }

    async fn append_account_version(&self, account: Account) -> Result<u64, StoreError> {
        // Pessimistic locking: inside one transaction, lock the account's head
        // row with `SELECT ... FOR UPDATE` so a concurrent appender waits here
        // until we commit, then check the version, append the new immutable row,
        // and move the head. `ON CONFLICT` is the portable backstop (SQLite has
        // no `FOR UPDATE`, and it covers the append phantom-insert a row lock
        // does not). The head is maintained by delete + insert, never `UPDATE`,
        // so the write path issues only inserts and deletes.
        let lock = self.dialect.lock_clause();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        // A guarded write: no such account, or a version that is not exactly one
        // past the head, matches nothing and reports 0. This is what keeps the
        // chain gap-free (a stale or skipped version never lands) and makes a
        // replay of an already-applied version a no-op.
        let current = sqlx::query(&format!(
            "SELECT version FROM account_head WHERE id = $1 AND subaccount = $2{lock}"
        ))
        .bind(account.id.id)
        .bind(account.id.sub)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| StoreError::Internal(e.to_string()))?;
        let Some(current) = current else {
            return Ok(0);
        };

        let current_version: i64 = current
            .try_get("version")
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let expected = current_version
            .checked_add(1)
            .ok_or_else(|| StoreError::Internal("account version overflow".to_string()))?;

        if account.version as i64 != expected {
            return Ok(0);
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
            return Ok(0);
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
        Ok(1)
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
