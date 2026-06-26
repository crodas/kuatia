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
| `postgres` | PostgreSQL via sqlx | Feature-flagged, needs running instance |

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
