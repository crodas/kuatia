//! Schema migrations. Idempotent: a `_migrations` ledger records what has been
//! applied, so re-running is a no-op. The DDL is identical for both backends.

use kuatia_storage::error::StoreError;

use crate::SqlStore;

impl SqlStore {
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
            (
                "007_balance_projection",
                include_str!("migrations/007_balance_projection.sql"),
            ),
            (
                "008_live_postings",
                include_str!("migrations/008_live_postings.sql"),
            ),
            (
                "009_saga_kind",
                include_str!("migrations/009_saga_kind.sql"),
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
