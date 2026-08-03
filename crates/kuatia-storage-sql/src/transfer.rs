//! [`TransferStore`]: committed envelope records and their account index.

use async_trait::async_trait;
use sqlx::Row;

use kuatia_storage::error::StoreError;
use kuatia_storage::store::*;
use kuatia_types::*;

use crate::SqlStore;
use crate::row::{deserialize_json, envelope_id_to_hex, serialize_json};

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
        // Push every predicate into SQL so the database returns only the
        // requested page, not the whole table (or the account's whole history).
        // This is what bounds the `balance()` tail scan by the watermark
        // (ADR-0019): `fold_tail` passes `from_ts = Some(watermark + 1)`, and
        // that lower bound now reaches the DB instead of being applied in Rust
        // after loading everything. Every bound is an `i64`, so they collect into
        // one ordered bind list. The account join is only added when an account
        // is requested (subaccount narrows within it).
        let from_clause = if query.account.is_some() {
            "FROM transfers t INNER JOIN transfer_accounts ta ON t.id = ta.transfer_id"
        } else {
            "FROM transfers t"
        };

        let mut conds: Vec<String> = Vec::new();
        let mut binds: Vec<i64> = Vec::new();
        let mut p = 1u32;
        if let Some(account) = query.account {
            conds.push(format!("ta.account_id = ${p}"));
            binds.push(account);
            p += 1;
            if let Some(sub) = query.sub {
                conds.push(format!("ta.subaccount = ${p}"));
                binds.push(sub);
                p += 1;
            }
        }
        if let Some(from) = query.from_ts {
            conds.push(format!("t.created_at >= ${p}"));
            binds.push(from);
            p += 1;
        }
        if let Some(to) = query.to_ts {
            conds.push(format!("t.created_at < ${p}"));
            binds.push(to);
            p += 1;
        }
        if let Some(book) = query.book {
            conds.push(format!("t.book = ${p}"));
            binds.push(book.0);
        }
        let where_sql = if conds.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conds.join(" AND "))
        };

        let count_sql = format!("SELECT COUNT(*) as cnt {from_clause}{where_sql}");
        let mut count_q = sqlx::query(&count_sql);
        for b in &binds {
            count_q = count_q.bind(*b);
        }
        let total: i64 = count_q
            .fetch_one(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?
            .try_get("cnt")
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        let limit = query.limit.unwrap_or(u32::MAX);
        let offset = query.offset.unwrap_or(0);
        let data_sql = format!(
            "SELECT t.transfer, t.receipt, t.created_at {from_clause}{where_sql} \
             ORDER BY t.created_at LIMIT {limit} OFFSET {offset}"
        );
        let mut data_q = sqlx::query(&data_sql);
        for b in &binds {
            data_q = data_q.bind(*b);
        }
        let rows = data_q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        let mut items = Vec::with_capacity(rows.len());
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
            items.push(EnvelopeRecord {
                envelope: deserialize_json(&transfer_json)?,
                receipt: deserialize_json(&receipt_json)?,
                created_at,
            });
        }
        Ok(Page {
            items,
            total: total as u64,
        })
    }
}
