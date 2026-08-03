//! [`BalanceProjectionStore`]: append-only balance cache points (ADR-0019).

use std::str::FromStr;

use async_trait::async_trait;
use sqlx::Row;

use kuatia_storage::error::StoreError;
use kuatia_storage::store::*;
use kuatia_types::*;

use crate::SqlStore;

#[async_trait]
impl BalanceProjectionStore for SqlStore {
    async fn append_balance_projection(
        &self,
        account: &AccountId,
        asset: &AssetId,
        balance: Cent,
        watermark: i64,
    ) -> Result<(), StoreError> {
        // Append-only: mint a fresh monotonic id and insert a new cache point.
        let id = self.autoid.next();
        sqlx::query(
            "INSERT INTO balance_projection (id, account, subaccount, asset, balance, watermark) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(account.id)
        .bind(account.sub)
        .bind(asset.0 as i32)
        .bind(balance.to_string())
        .bind(watermark)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn get_closest_balance_projection(
        &self,
        account: &AccountId,
        asset: &AssetId,
        as_of: i64,
    ) -> Result<Option<BalanceProjection>, StoreError> {
        // Closest at or before `as_of`: the largest watermark not exceeding it,
        // tie-broken by highest id. Row selection, not an aggregate over values.
        let row = sqlx::query(
            "SELECT id, balance, watermark FROM balance_projection \
             WHERE account = $1 AND subaccount = $2 AND asset = $3 AND watermark <= $4 \
             ORDER BY watermark DESC, id DESC LIMIT 1",
        )
        .bind(account.id)
        .bind(account.sub)
        .bind(asset.0 as i32)
        .bind(as_of)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| StoreError::Internal(e.to_string()))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let id: i64 = row
            .try_get("id")
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let balance: String = row
            .try_get("balance")
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let watermark: i64 = row
            .try_get("watermark")
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(Some(BalanceProjection {
            id,
            account: *account,
            asset: *asset,
            balance: Cent::from_str(&balance).map_err(|e| StoreError::Internal(e.to_string()))?,
            watermark,
        }))
    }
}
