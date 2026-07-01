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

A single portable schema in `src/migrations/001_init.sql` serves both backends.
Applied migrations are tracked in a `_migrations` table, so `migrate()` is
idempotent. Upserts use portable `ON CONFLICT … DO UPDATE`, and all ids are
generated in Rust (no `AUTOINCREMENT`/`SERIAL`).

Every column is a text type: no opaque binary is stored. Content-addressed ids
(and the opaque saga blob) are kept as lower-case hex `TEXT`, and structured
payloads as their JSON `TEXT` serialization, so any row is readable in a plain
SQL client for auditing. The JSON is never queried into.

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
