# kuatia-storage

Storage abstraction for the kuatia ledger.

Defines the `Store` trait (composed of seven sub-traits), provides an
in-memory implementation for tests, and exports a `store_tests!` conformance
macro that any backend can use to validate its implementation.

## Sub-traits

| Trait | Purpose |
|-------|---------|
| `AccountStore` | Account CRUD and versioning |
| `PostingStore` | Posting reads + lifecycle: `reserve`/`release`/`deactivate`/`insert` postings (reserve/release/deactivate carry a `ReservationId`) |
| `TransferStore` | Transfer persistence (`store_transfer`) and queries |
| `SagaStore` | Saga state for crash recovery |
| `EventStore` | Append-only ledger event log (idempotent on a per-transfer dedup key) |
| `BookStore` | Book (transfer policy scope) persistence |

The store is a **dumb instruction follower**: write methods apply one update and
return the **number of affected rows** (or an I/O error). They do not interpret
counts, decide state, enforce idempotency, or compensate — the saga in the
`kuatia` crate does. There is no `commit_transfer`; commit is a sequence of these
primitives, each idempotent.

`Store` is a blanket trait — any type implementing the sub-traits is a `Store`.

## Conformance testing

```rust
use kuatia_storage::mem_store::InMemoryStore;

async fn new_store() -> InMemoryStore { InMemoryStore::new() }
kuatia_storage::store_tests!(new_store);
```

This generates a test for every Store method, run against any backend.
