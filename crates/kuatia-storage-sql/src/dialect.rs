//! The SQL dialect seam: the one place the SQLite/PostgreSQL divergence lives.
//!
//! Resolved once at construction from the pool's connection URL (no query), so
//! the write paths read a plain enum instead of re-probing the backend. A third
//! backend becomes a new variant here, not edits across every `impl`.

use sqlx::{Any, Pool};

/// Which SQL backend a [`SqlStore`](crate::SqlStore) is talking to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dialect {
    /// PostgreSQL: supports `SELECT ... FOR UPDATE` row locking.
    Postgres,
    /// SQLite: no `FOR UPDATE`; it serializes writers itself.
    Sqlite,
}

impl Dialect {
    /// Resolve the dialect from the pool's connection URL scheme. Synchronous
    /// and issues no query. Anything that is not `sqlite` is treated as
    /// PostgreSQL, matching the prior runtime probe (which classified any
    /// non-SQLite backend as Postgres).
    pub(crate) fn from_pool(pool: &Pool<Any>) -> Self {
        match pool.connect_options().database_url.scheme() {
            "sqlite" => Self::Sqlite,
            _ => Self::Postgres,
        }
    }

    /// Row-locking clause appended to a `SELECT` that takes a pessimistic lock:
    /// ` FOR UPDATE` on Postgres, empty on SQLite (which has no such clause and
    /// serializes writers itself).
    pub(crate) fn lock_clause(self) -> &'static str {
        match self {
            Self::Postgres => " FOR UPDATE",
            Self::Sqlite => "",
        }
    }
}
