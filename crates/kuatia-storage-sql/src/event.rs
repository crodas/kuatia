//! [`EventStore`]: the append-only ledger event log, deduped on a stable key.

use async_trait::async_trait;
use sqlx::Row;

use kuatia_storage::error::StoreError;
use kuatia_storage::events::{EventStore, LedgerEvent, event_dedup_key};

use crate::SqlStore;
use crate::row::{deserialize_json, serialize_json};

#[async_trait]
impl EventStore for SqlStore {
    async fn append_event(&self, event: &LedgerEvent) -> Result<u64, StoreError> {
        let kind_str =
            serde_json::to_string(&event.kind).map_err(|e| StoreError::Internal(e.to_string()))?;
        let data = serialize_json(event)?;
        let seq = self.autoid.next() as u64;

        // Idempotent on the dedup key: a replayed transfer or lifecycle-transition
        // event conflicts on `dedup_key` and returns the existing seq instead of a
        // duplicate row.
        match event_dedup_key(&event.kind) {
            Some(dedup_key) => {
                let res = sqlx::query("INSERT INTO events (seq, timestamp, kind, data, dedup_key) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (dedup_key) DO NOTHING")
                    .bind(seq as i64)
                    .bind(event.timestamp)
                    .bind(&kind_str)
                    .bind(&data)
                    .bind(&dedup_key)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                if res.rows_affected() == 0 {
                    let row = sqlx::query("SELECT seq FROM events WHERE dedup_key = $1")
                        .bind(&dedup_key)
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
