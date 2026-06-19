# kuatia-storage-sql

SQL-backed `Store` implementation for the kuatia ledger.

Uses `sqlx::Any` for database-agnostic queries. Enable features to select
the backend:

```toml
[dependencies]
kuatia-storage-sql = { features = ["sqlite"] }   # or "postgres"
```

## Backends

| Feature | Backend | Status |
|---------|---------|--------|
| `sqlite` (default) | SQLite via sqlx | Conformance tests pass |
| `postgres` | PostgreSQL via sqlx | Portable DDL/queries; needs a running instance to test |

The backend is detected at migration time and the matching DDL is applied from
`src/migrations/{sqlite,postgres}/` (SQLite uses `BLOB`, PostgreSQL uses
`BYTEA`). Applied migrations are tracked in a `_migrations` table, so
`migrate()` is idempotent. Upserts use portable `ON CONFLICT … DO UPDATE`, and
all ids are generated in Rust (no `AUTOINCREMENT`/`SERIAL`).

## Usage

```rust
use kuatia_storage_sql::SqlStore;

let pool = sqlx::any::AnyPoolOptions::new()
    .connect("sqlite::memory:").await?;
let store = SqlStore::new(pool);
store.migrate().await?;
```

## Schema

Tables: `accounts`, `postings`, `transfers`, `transfer_accounts`, `sagas`,
`events`, `books`. Migrations run via `store.migrate()`.
