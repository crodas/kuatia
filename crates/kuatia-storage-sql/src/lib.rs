//! SQL-backed Store implementation for SQLite and PostgreSQL.
//!
//! Uses `sqlx::Any` for database-agnostic queries. Enable features
//! `sqlite` or `postgres` to select the backend.
//!
//! ```text
//! let pool = sqlx::any::AnyPoolOptions::new()
//!     .connect("sqlite::memory:").await?;
//! let store = SqlStore::new(pool);
//! store.migrate().await?;
//! ```
//!
//! The [`Store`](kuatia_storage::store::Store) sub-traits are each implemented in
//! their own module (`account`, `posting`, `transfer`, `saga`, `event`, `book`,
//! `projection`); shared row mappers and codecs live in `row`, the schema
//! migrations in `migrate`, and the one SQLite/PostgreSQL divergence behind the
//! `Dialect` seam in `dialect`.

use sqlx::{Any, Pool};

use kuatia_types::autoid::AutoId;

use crate::dialect::Dialect;

mod account;
mod book;
mod dialect;
mod event;
mod migrate;
mod posting;
mod projection;
mod row;
mod saga;
mod transfer;

/// SQL-backed [`Store`](kuatia_storage::store::Store) implementation.
pub struct SqlStore {
    pool: Pool<Any>,
    autoid: AutoId,
    /// Which backend this store talks to; resolved once at construction.
    dialect: Dialect,
}

impl SqlStore {
    /// Create a new SQL store wrapping an existing connection pool. The backend
    /// dialect is resolved from the pool's connection URL, so no query is issued
    /// here; call [`migrate`](Self::migrate) next to apply the schema.
    pub fn new(pool: Pool<Any>) -> Self {
        let dialect = Dialect::from_pool(&pool);
        Self {
            pool,
            autoid: AutoId::new(),
            dialect,
        }
    }
}
