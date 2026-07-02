# kuatia-dashboard

A read-only visualizer for a Kuatia ledger. It connects to a ledger database
(SQLite or PostgreSQL) and serves two views of it:

- A server-rendered HTML dashboard (Tera templates, htmx for live refresh).
- A JSON REST API under `/api` for anyone who wants to build a richer client.

The dashboard only observes the ledger. It never mutates it. Templates, CSS,
and htmx are embedded in the binary, so nothing extra is needed on disk at
runtime.

## Run it

```sh
# In-memory demo (seeds accounts/transfers on start):
cargo run -p kuatia-dashboard -- --seed
# open http://127.0.0.1:3000

# Persist to a SQLite file, seed once, then reopen without reseeding:
cargo run -p kuatia-dashboard -- --db sqlite://kuatia.db --seed
cargo run -p kuatia-dashboard -- --db sqlite://kuatia.db

# Point at a PostgreSQL ledger on a custom port:
cargo run -p kuatia-dashboard -- --db postgres://user:pass@host/db --port 8080
```

`--seed` populates demo data only when the ledger is empty, so it is safe to
leave on; against an already-populated database it is a no-op.

## Pages

| Path             | Shows                                                      |
| ---------------- | --------------------------------------------------------- |
| `/`              | Overview: account count, transfer count, issued per asset |
| `/accounts`      | All accounts with policy, flags, and balances             |
| `/accounts/:id`  | One account: balances, postings, and its transfers        |
| `/transfers`     | Recent transfers with their created postings              |
| `/events`        | The append-only ledger event log                          |

Each page wraps its dynamic section in an htmx `hx-get`/`hx-trigger="every 3s"`
element that polls a matching `/ui/*` route and swaps in the fresh fragment.

## REST API

All amounts are minor-unit strings (the ledger's native `Cent` form). Clients
format them using the decimals from `/api/assets`.

| Method & path           | Returns                                               |
| ----------------------- | ----------------------------------------------------- |
| `GET /api/assets`       | Asset registry: id, code, symbol, decimals            |
| `GET /api/overview`     | Account count, transfer count, total issued per asset |
| `GET /api/accounts`     | All accounts with policy, flags, and balances         |
| `GET /api/accounts/:id` | One account plus its postings and transfers           |
| `GET /api/transfers`    | Recent transfers with their created postings (legs)   |
| `GET /api/events`       | The append-only ledger event log                      |

## Configuration

Each option is a CLI flag with an environment-variable fallback and a default.

| Flag       | Env var                 | Default          | Purpose                                  |
| ---------- | ----------------------- | ---------------- | ---------------------------------------- |
| `--db`     | `KUATIA_DASHBOARD_DB`   | `sqlite::memory:`| Ledger database URL (SQLite or Postgres) |
| `--host`   | `KUATIA_DASHBOARD_HOST` | `127.0.0.1`      | Interface to bind                        |
| `--port`   | `KUATIA_DASHBOARD_PORT` | `3000`           | Listen port                              |
| `--seed`   | `KUATIA_DASHBOARD_SEED` | `false`          | Seed demo data if the ledger is empty    |
| (n/a)      | `RUST_LOG`              | `kuatia_dashboard=info,tower_http=info` | Log filter                |
