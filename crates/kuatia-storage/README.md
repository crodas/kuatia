# kuatia-storage

Storage abstraction for the kuatia ledger.

Defines the `Store` trait (composed of seven sub-traits), provides an
in-memory implementation for tests, and exports a `store_tests!` conformance
macro that any backend can use to validate its implementation.

## Sub-traits

| Trait | Purpose |
|-------|---------|
| `AccountStore` | Account CRUD and versioning |
| `PostingStore` | Posting reads, reserve/release/finalize lifecycle (reserve/release carry a `ReservationId`) |
| `TransferStore` | Transfer persistence and queries |
| `SagaStore` | Saga state for crash recovery |
| `EventStore` | Append-only ledger event log |
| `BookStore` | Book (transfer policy scope) persistence |
| `CommitStore` | `commit_transfer` — the single atomic commit boundary (postings + transfer + index + events) |

`Store` is a blanket trait — any type implementing all seven sub-traits is a `Store`.

## Conformance testing

```rust
use kuatia_storage::mem_store::InMemoryStore;

async fn new_store() -> InMemoryStore { InMemoryStore::new() }
kuatia_storage::store_tests!(new_store);
```

This generates a test for every Store method, run against any backend.
