# kuatia

Async ledger resource — the main entry point for callers.

Composes `kuatia-core` (validation) and `kuatia-storage` (persistence) into
a saga-driven commit pipeline with automatic retry and compensation.

## API layers

### Intent layer (highest level)

Build transfers with `TransferBuilder`, then commit them:

```rust
let transfer = TransferBuilder::new()
    .deposit(alice, usd, Cent::from(100), bank)
    .build();
let receipt = ledger.commit(transfer).await?;
```

| Builder method | Description |
|---------------|-------------|
| `.pay(from, to, asset, amount)` | Transfer with automatic posting selection and change |
| `.deposit(to, asset, amount, external)` | Fund an account from an external source |
| `.withdraw(from, asset, amount, external)` | Send value to an external destination |
| `.movement(from, to, asset, amount)` | Raw movement for custom operations |

### Commit

Every commit is the **envelope saga** — two steps driven by `legend` with
automatic retry and LIFO compensation:

- `commit(transfer)` — resolves the intent into a concrete envelope (read-only),
  then runs `commit_envelope`.
- `commit_envelope(envelope)` — the one commit path. Persists a write-ahead
  `PendingSaga` record (phase `Reserving`), then:
  1. **Reserve** — `reserve_postings`: Active → PendingInactive, stamped with this saga's `ReservationId`
  2. **Finalize** — re-validates against current state (the last-step floor / freeze-close guard), marks the saga `Finalizing`, then runs the dumb primitives `deactivate_postings` → `insert_postings` → `store_transfer` → `append_event`, verifying every end-state
- `reverse(id)` — builds a reversal envelope and runs the same path.

The store reports an **affected-row count** for each primitive; the saga
interprets it (full = continue, partial = error → compensate, zero = read state
and continue only if this same envelope already applied it). There is no
monolithic `commit_transfer` and no separate "atomic" path.

### Crash recovery

`recover()` — call on startup. It completes any `PendingSaga` left by a crash,
branching on the persisted phase: a `Reserving` saga is re-run (re-validating,
aborting cleanly if a posting was taken or an account frozen); a `Finalizing`
saga is rolled forward through the verified `finalize_envelope`. Roll-forward,
not rollback.

### Account lifecycle

| Method | Description |
|--------|-------------|
| `create_account(account)` | Create account and emit AccountCreated event |
| `freeze(id)` | Set FROZEN flag |
| `unfreeze(id)` | Clear FROZEN flag |
| `close(id)` | Set CLOSED flag (requires zero active postings) |

### Queries

| Method | Description |
|--------|-------------|
| `balance(account, asset)` | Current balance (sum of non-Inactive postings) |
| `query_transfers(query)` | Paginated, filtered transfer history |
| `history(account)` | All transfers for an account |
| `postings(account)` | All postings (any status) |
| `get_events_since(seq, limit)` | Query ledger event log |

### Saga composition

Combine steps into multi-transfer workflows using the `legend!` macro:

```rust
legend! {
    FundAndPay<LedgerCtx, SagaError> {
        deposit: DepositMovementStep,
        pay: PayMovementStep,
    }
}
```

## Examples

Runnable programs in [`examples/`](examples/) connect to a real SQLite-backed
ledger (via `sqlx`) and walk through the core operations:

```sh
cargo run -p kuatia --example create_accounts   # create user/system/external accounts
cargo run -p kuatia --example fund_and_trade     # fund two accounts in different assets, then swap
cargo run -p kuatia --example withdraw           # fund an account, then withdraw out of the ledger
```

Each opens an in-memory SQLite database (`sqlite::memory:`); point the
connection string at a file or a Postgres URL for a persistent ledger.

## See also

- [doc/accounting-mapping.md](../../doc/accounting-mapping.md) — how classical
  double-entry concepts (journal, journal entry, ledger) map onto kuatia's
  transfer log, transfers, and postings.
