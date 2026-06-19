# Kuatia — Project Context

## What is this

Kuatia is an append-only, auditable, multi-asset UTXO-style ledger library in Rust. Value is tracked as signed postings — no mutable balance fields. Transfers atomically consume and create postings, enforcing per-asset conservation — the double-entry-style safety invariant (`sum(consumed) == sum(created)` per asset).

## Crate layout

```
crates/
  kuatia-money/     Cent monetary type + CentBacking trait; integer width (i64 default, i128 via feature) is hidden and swappable
  kuatia-types/     Domain types: AccountId, Posting, Movement, AutoId, etc.; re-exports Cent/Amount from kuatia-money
  kuatia-core/      Pure, sync, no-IO logic: validation, hashing, posting selection
  kuatia-storage/   Store trait (7 sub-traits), InMemoryStore, conformance tests
  kuatia-storage-sql/  SQL backend: SQLite/PostgreSQL via sqlx
  kuatia/           Async layer: Ledger resource, saga pipeline, intent API
doc/
  architecture.md   Architecture decisions and rationale
  crates.md         Crate reference: modules, types, APIs
  accounts.md       Account model, policies, lifecycle
  transfers.md      Transfer/Movement API, resolve algorithm
  glossary.md       Terms, book design, exchange & supermarket examples
  accounting-mapping.md  Classical double-entry ↔ Kuatia term mapping
```

## Key concepts

- **Posting**: signed amount of one asset owned by one account. Lifecycle: Active → PendingInactive → Inactive.
- **Movement**: `{ from, to, asset, amount }` — the fundamental unit of intent. All operations (pay, deposit, withdraw) are one or more movements.
- **Envelope**: concrete postings to consume and create — the resolved form of movements.
- **Conservation**: for each asset, `sum(consumed) == sum(created)`.
- **Account policies**: NoOverdraft, CappedOverdraft, UncappedOverdraft, SystemAccount, ExternalAccount. Only `NoOverdraft` forbids negative postings; the other four permit them. An overdraft is a negative posting that covers a shortfall — down to the floor for `CappedOverdraft`, unbounded for `UncappedOverdraft`.
- **Dumb storage**: the `Store` is a thin instruction follower. Write methods apply one update and return the **number of affected rows** (or an I/O error) — they never interpret counts, decide state, enforce idempotency, or compensate. The saga owns all of that. There is no monolithic `commit_transfer`; commit is a sequence of dumb primitives (`reserve_postings`, `deactivate_postings`, `insert_postings`, `store_transfer`, `append_event`), each idempotent. See [doc/adr/0003-dumb-storage-saga-recovery.md](doc/adr/0003-dumb-storage-saga-recovery.md).

## Architecture

- **Pure core / async layer separation**: kuatia-core has zero IO, fully deterministic, testable with golden vectors. kuatia adds async Store trait and saga pipeline.
- **Saga commit pipeline**: every commit is the **two-step** envelope saga `reserve → finalize` (validation runs inside the finalize step, as the last thing before the writes), with automatic retry and LIFO compensation via the `legend` crate. `commit(transfer)` = resolve (read-only) then `commit_envelope`; `reverse()` builds a reversal envelope and runs the same path. There is one commit path, not a separate "atomic" one.
- **Count interpretation**: the saga reads each primitive's affected-row count — full = continue; partial = error → compensate; zero = read state and continue only if this same envelope/reservation already applied it (idempotency). `finalize_envelope` additionally verifies every end-state (all consumed postings `Inactive`, created exist, transfer stored).
- **Durable recovery**: a phase-tracked write-ahead `PendingSaga {envelope, reservation, phase}` is persisted via `SagaStore` before the saga mutates anything (`Reserving`), bumped to `Finalizing` once validation passed and the consumed postings are about to turn `Inactive`. `Ledger::recover()` (call on startup) branches on phase: a `Reserving` saga is **re-run and re-validated** (aborting cleanly if a posting was taken or an account frozen); a `Finalizing` saga is rolled forward through the verified `finalize_envelope`. Roll-forward, not rollback, so there are no orphaned `PendingInactive` postings to reconcile.
- **Content-addressed transfers**: EnvelopeId = double-SHA-256 of canonical bytes. Provides idempotency and tamper evidence.
- **Append-only accounts**: versioned, never modified in place. Snapshot pinning (validate-time) prevents TOCTOU races; under the dumb-storage model the overdraft-floor and freeze/close guards are validate-time and best-effort under concurrency.
- **Store uses `Arc<dyn Store>`**: Ledger is non-generic, enabling concrete saga types.

## Resolve algorithm

Two-pass:
1. For each movement, create output posting on `to` and accumulate net debit on `from`.
2. For each (account, asset) with positive net debit, select postings (greedy largest-first) and compute change. If positive postings are insufficient: `CappedOverdraft`/`UncappedOverdraft` accounts consume all positives and create a negative posting for the shortfall (floor enforced in validation); other policies fail with `InsufficientFunds`.

Deposit: two movements cancel to zero net debit on the system account — no posting selection needed.

## Validation steps (validate_and_plan)

1. Non-empty
2. No duplicate consumed PostingIds
3. Consumed postings exist
4. Consumed postings Active or PendingInactive
5. Referenced accounts exist, not frozen, not closed
6. Account snapshot pinning
7. Book policy (if a book is loaded): referenced assets/accounts/flags allowed by the book
8. Per-asset conservation
9. Negative postings forbidden only on `NoOverdraft` (allowed on overdraft/system/external)
10. Policy enforcement (balance floor)

## Testing

```bash
cargo test          # runs all tests across all crates
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
  - Bits 62–23: milliseconds since `KUATIA_EPOCH_MS` (2026-01-01T00:00:00Z), not the Unix epoch — 40 bits ≈ 34.8 years going forward (until ~2060)
  - Bits 22–0: lower 23 bits of CRC32 of context-specific data (e.g. serialized event)
  - When no data is provided, an internal atomic counter is used (wraps on 23-bit overflow)
  - Implementation: `AutoId` in `kuatia-types/src/autoid.rs`, includes inline CRC32 (IEEE)
  - Generated in Rust, stored as plain `BIGINT` — the DB never assigns IDs
