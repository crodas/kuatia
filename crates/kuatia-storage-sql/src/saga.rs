//! [`SagaStore`]: opaque write-ahead saga records stored as hex `TEXT`.

use async_trait::async_trait;
use sqlx::Row;

use kuatia_storage::error::StoreError;
use kuatia_storage::store::*;

use crate::SqlStore;
use crate::row::{from_hex, to_hex};

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

    async fn get_saga(&self, id: &i64) -> Result<Option<Vec<u8>>, StoreError> {
        let row = sqlx::query("SELECT data FROM sagas WHERE id = $1")
            .bind(*id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        match row {
            Some(row) => {
                let data_hex: String = row
                    .try_get("data")
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                Ok(Some(from_hex(&data_hex)?))
            }
            None => Ok(None),
        }
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
