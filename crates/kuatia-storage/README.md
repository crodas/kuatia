# kuatia-storage

Storage abstraction for the kuatia ledger.

Defines the `Store` trait (composed of seven sub-traits), provides an
in-memory implementation for tests, and exports a `store_tests!` conformance
macro that any backend can use to validate its implementation.

## Sub-traits

| Trait | Purpose |
|-------|---------|
| `AccountStore` | Account CRUD and versioning |
| `PostingStore` | Posting reads + generic lifecycle primitives (`reserve`/`release`/`deactivate`/`insert`) |
| `TransferStore` | Transfer persistence (`store_transfer`) and queries |
| `CommitStore` | The atomic commit boundary: `commit_envelope` / `commit_transition` (ADR-0023) |
| `EventStore` | Append-only ledger event log (idempotent on a per-transfer dedup key) |
| `BookStore` | Book (transfer policy scope) persistence |

A transfer is committed atomically through `CommitStore::commit_envelope`, which
applies the whole envelope in one transaction and re-checks double-spend,
freeze/close, and the overdraft floor inside it. The remaining posting/account
write methods are **dumb instructions**: each applies one update and returns the
**number of affected rows** (or an I/O error), never interpreting the count.

`Store` is a blanket trait — any type implementing the sub-traits is a `Store`.

## Conformance testing

```rust
use kuatia_storage::mem_store::InMemoryStore;

async fn new_store() -> InMemoryStore { InMemoryStore::new() }
kuatia_storage::store_tests!(new_store);
```

This generates a test for every Store method, run against any backend.
