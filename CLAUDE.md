# Kuatia — Project Context

## What is this

Kuatia is an append-only, auditable, multi-asset UTXO-style ledger library in Rust. Value is tracked as signed postings — no mutable balance fields. Transfers atomically consume and create postings, enforcing per-asset conservation (double-entry bookkeeping).

## Crate layout

```
crates/
  kuatia-types/     Domain types: AccountId, Posting, Movement, Cent, AutoId, etc.
  kuatia-core/      Pure, sync, no-IO logic: validation, hashing, posting selection
  kuatia-storage/   Store trait (5 sub-traits), InMemoryStore, conformance tests
  kuatia-storage-sql/  SQL backend: SQLite/PostgreSQL via sqlx
  kuatia/           Async layer: Ledger resource, saga pipeline, intent API
doc/
  architecture.md   Architecture decisions and rationale
  crates.md         Crate reference: modules, types, APIs
  accounts.md       Account model, policies, lifecycle
  transfers.md      Transfer/Movement API, resolve algorithm
  glossary.md       Terms, journal design, exchange & supermarket examples
```

## Key concepts

- **Posting**: signed amount of one asset owned by one account. Lifecycle: Active → PendingInactive → Inactive.
- **Movement**: `{ from, to, asset, amount }` — the fundamental unit of intent. All operations (pay, deposit, withdraw) are one or more movements.
- **Envelope**: concrete postings to consume and create — the resolved form of movements.
- **Conservation**: for each asset, `sum(consumed) == sum(created)`.
- **Account policies**: NoOverdraft, CappedOverdraft, UncappedOverdraft, SystemAccount, ExternalAccount. Only SystemAccount and ExternalAccount may hold negative postings.

## Architecture

- **Pure core / async layer separation**: kuatia-core has zero IO, fully deterministic, testable with golden vectors. kuatia adds async Store trait and saga pipeline.
- **Saga commit pipeline**: reserve → validate → finalize, with automatic retry and LIFO compensation via the `legend` crate.
- **Content-addressed transfers**: EnvelopeId = double-SHA-256 of canonical bytes. Provides idempotency and tamper evidence.
- **Append-only accounts**: versioned, never modified in place. Snapshot pinning prevents TOCTOU races.
- **Store uses `Arc<dyn Store>`**: Ledger is non-generic, enabling concrete saga types.

## Resolve algorithm

Two-pass:
1. For each movement, create output posting on `to` and accumulate net debit on `from`.
2. For each (account, asset) with positive net debit, select postings (greedy largest-first) and compute change.

Deposit: two movements cancel to zero net debit on the system account — no posting selection needed.

## Validation steps (validate_and_plan)

1. Non-empty
2. No duplicate consumed PostingIds
3. Consumed postings exist
4. Consumed postings Active or PendingInactive
5. Referenced accounts exist, not frozen, not closed
6. Account snapshot pinning
7. Per-asset conservation
8. Negative postings only on SystemAccount/ExternalAccount
9. Policy enforcement (balance floor)

## Testing

```bash
cargo test          # runs all 119 tests across all crates
cargo test -p kuatia-core   # pure core tests only
cargo test -p kuatia        # integration + saga tests
```

## Conventions

- Clarity over cleverness
- **All arithmetic in Rust only** — the storage layer is a dumb record keeper. No SQL `SUM`, `MAX`, `MIN`, `AVG`, or any computation on monetary amounts or domain values in queries. `COUNT(*)` for pagination row totals is allowed (it counts rows, not domain values). Balances are always computed in Rust with checked arithmetic (`checked_add`, `checked_sub`, `checked_neg`) — no silent overflow
- No `unwrap()`/`expect()` in production code — all errors bubble up via `Result`
- Domain types for all identifiers — never raw integers or byte arrays in public APIs
- Use "Posting" not "Coin" for accounting clarity
- TransferBuilder convenience methods (`.pay()`, `.deposit()`, `.withdraw()`) over raw `.movement()` construction
- Every Store sub-trait method must have a conformance test in `store_tests!` macro — new trait methods require new tests
- `.deposit()` returns `Result<Self, OverflowError>` — callers must handle the error
- **No AUTOINCREMENT / SERIAL in the database** — all IDs are generated in Rust. Use snowflake-style `i64` IDs with the following bit layout:
  ```
  [0][  40 bits: ms timestamp  ][ 23 bits: CRC32(data) ]
   ^sign (always 0 = positive)
  ```
  - Bit 63: always 0 (keeps i64 positive)
  - Bits 62–23: Unix milliseconds (40 bits ≈ 34.8 years from epoch)
  - Bits 22–0: lower 23 bits of CRC32 of context-specific data (e.g. serialized event)
  - When no data is provided, an internal atomic counter is used (wraps on 23-bit overflow)
  - Implementation: `AutoId` in `kuatia-types/src/autoid.rs`, includes inline CRC32 (IEEE)
  - Generated in Rust, stored as plain `BIGINT` — the DB never assigns IDs
