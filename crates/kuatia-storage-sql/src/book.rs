//! [`BookStore`]: book definitions stored as JSON `TEXT`.

use async_trait::async_trait;
use sqlx::Row;

use kuatia_storage::error::StoreError;
use kuatia_storage::store::*;
use kuatia_types::*;

use crate::SqlStore;
use crate::row::{deserialize_json, serialize_json};

#[async_trait]
impl BookStore for SqlStore {
    async fn create_book(&self, book: Book) -> Result<u64, StoreError> {
        // Pessimistic locking, same shape as create_account: lock any existing
        // book row with `SELECT ... FOR UPDATE` inside the transaction, then
        // insert with `ON CONFLICT DO NOTHING` as the portable backstop.
        let lock = self.dialect.lock_clause();
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
            return Ok(0);
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
            return Ok(0);
        }

        tx.commit()
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(1)
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
