# kuatia

Async ledger resource — the main entry point for callers.

Composes `kuatia-core` (validation) and `kuatia-storage` (persistence) into an
atomic commit engine: resolve, validate, then apply the whole transfer in one
store transaction.

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

Every commit is **atomic** — one store transaction:

- `commit(transfer)` — resolves the intent into a concrete envelope (read-only),
  then runs `commit_envelope`.
- `commit_envelope(envelope)` — the one commit path. Validates against loaded
  state in the pure core, then calls `store.commit_envelope(..)`, which in one
  transaction spends the consumed postings, inserts the created ones, stores the
  transfer, and appends the committed event — re-checking double-spend,
  freeze/close, and the overdraft floor inside it.
- `reverse(id)` — builds a reversal envelope and runs the same path.

A domain refusal (double-spend, frozen, closed, overdraft) comes back as a typed
`LedgerError`, and nothing is applied. Because the write is all-or-nothing, a
crash leaves no half-applied state — there is no write-ahead log and no recovery
step.

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

### Multi-transfer composition

Combine transfers into an all-or-nothing workflow with `run_movements`, which
commits them in order and reverses the committed ones LIFO on failure:

```rust
use kuatia::saga::{DepositInput, Movement, PayInput, run_movements};

let receipts = run_movements(&ledger, &[
    Movement::Deposit(DepositInput { to: alice, asset: usd, amount, external: bank }),
    Movement::Pay(PayInput { from: alice, to: bob, asset: usd, amount }),
])
.await?;
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
