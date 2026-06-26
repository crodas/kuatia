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

### Saga commit

`commit(transfer)` runs the four-step pipeline with automatic compensation:
1. **Resolve** — convert Transfer intent into concrete Envelope
2. **Reserve** — batch CAS: Active → PendingInactive
3. **Validate** — pure `validate_and_plan()`
4. **Finalize** — PendingInactive → Inactive, create new postings, emit event

### Atomic commit

`commit_atomic(envelope)` — single-pass load → plan → apply. Used by `reverse()`.

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

## See also

- [doc/accounting-mapping.md](../../doc/accounting-mapping.md) — how classical
  double-entry concepts (journal, journal entry, ledger) map onto kuatia's
  transfer log, transfers, and postings.
