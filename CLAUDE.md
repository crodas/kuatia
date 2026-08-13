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
  kuatia/           Async layer: Ledger resource, atomic commit engine, intent API
doc/
  architecture.md   Architecture decisions and rationale
  crates.md         Crate reference: modules, types, APIs
  accounts.md       Account model, balance constraint, lifecycle
  transfers.md      Transfer/Movement API, resolve algorithm
  journaling.md     Journaling: transfers as (compound) journal entries
  glossary.md       Terms, book design, exchange & supermarket examples
  accounting-mapping.md  Classical double-entry ↔ Kuatia term mapping
```

## Key concepts

- **Posting**: signed amount of one asset owned by one account. Lifecycle: Active → PendingInactive → Inactive.
- **Movement**: `{ from, to, asset, amount }` — the fundamental unit of intent. All operations (pay, deposit, withdraw) are one or more movements.
- **Envelope**: concrete postings to consume and create — the resolved form of movements.
- **Conservation**: for each asset, `sum(consumed) == sum(created)`.
- **Balance constraint**: one per-account flag, `AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT`. Default (no flag): overdraft allowed, unbounded, a shortfall becomes a negative offset posting and the transfer records if it conserves value. Flag set: balance may not go negative and the account may not hold a negative posting. Construct with `Account::debit_must_not_exceed_credit(id)`; query with `Account::forbids_overdraft()`. There is no bounded floor and no system/external policy label; a boundary/deposit account is just a default overdraft-permitting account.
- **Atomic storage commit**: the `Store` applies a whole transfer in one transaction via `CommitStore::commit_envelope` (and a whole account-version transition via `commit_transition`). The pure core validates first; the transaction re-checks the stateful guards inside it — double-spend, freeze/close, and the overdraft floor — so they are strict, not best-effort. `commit(transfer)` = resolve (read-only) then `commit_envelope`; `reverse()` builds a reversal envelope and runs the same path. A crash leaves no half-applied state, so there is no write-ahead log and no recovery. See [doc/adr/0023-atomic-storage-commit.md](doc/adr/0023-atomic-storage-commit.md) (supersedes 0003). Reads and the `insert_postings`/`reserve_postings`/`release_postings`/`deactivate_postings` posting primitives remain as dumb single-update instructions returning affected-row counts.

## Architecture

- **Pure core / async layer separation**: kuatia-core has zero IO, fully deterministic, testable with golden vectors. kuatia adds the async Store trait and the commit engine.
- **Atomic commit path**: `commit(transfer)` = resolve (read-only) → validate in the pure core → `store.commit_envelope(..)`, one transaction that spends the consumed postings, inserts the created ones, stores the transfer, and appends the committed event. `reverse()` builds a reversal envelope and runs the same path. There is one commit path.
- **In-transaction guards**: the store re-checks double-spend (the delete-affected-count is the atomic single-winner claim), freeze/close (over the involved accounts), and the overdraft floor (summed in Rust from live rows, never in SQL) inside the commit transaction. A domain rejection is a typed `CommitRejection` the ledger maps back to a `LedgerError`.
- **No recovery**: because a commit is all-or-nothing, a crash leaves no half-applied state. There is no `PendingSaga`, no `SagaStore`, and no `Ledger::recover()`. Multi-transfer composition is a plain LIFO runner (`saga::run_movements`) over `commit`/`reverse`, not a saga VM.
- **Content-addressed transfers**: EnvelopeId = double-SHA-256 of canonical bytes. Provides idempotency and tamper evidence.
- **Append-only accounts**: versioned, never modified in place. Snapshot pinning (validate-time) prevents TOCTOU races; the no-overdraft (zero-floor) and freeze/close guards are re-checked strictly inside the atomic commit transaction (ADR-0023), not just at validate time.
- **Store uses `Arc<dyn Store>`**: Ledger is non-generic.

## Resolve algorithm

Two-pass:
1. For each movement, create output posting on `to` and accumulate net debit on `from`.
2. For each (account, asset) with positive net debit, select postings (greedy largest-first) and compute change. If positive postings are insufficient: overdraft-permitting accounts (no `DEBIT_MUST_NOT_EXCEED_CREDIT` flag) consume all positives and create a negative posting for the shortfall; accounts that forbid overdraft fail with `InsufficientFunds`.

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
9. Negative postings forbidden only on accounts with `DEBIT_MUST_NOT_EXCEED_CREDIT` (allowed on overdraft-permitting accounts)
10. Zero-floor enforcement for accounts that forbid overdraft

## Testing

```bash
cargo test          # runs all tests across all crates
cargo test -p kuatia-core   # pure core tests only
cargo test -p kuatia        # integration + commit/concurrency tests
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
