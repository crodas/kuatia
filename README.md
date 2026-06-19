# kuatia

> **kuatia** (kuatiʼa) — Guaraní for *paper*, *document*, *writing*.
> A fitting name for a small, append-only ledger library.

Auditable, multi-asset UTXO-style ledger in Rust.

## Overview

kuatia models value as **postings** — signed amounts owned by exactly one account.
Transfers atomically consume existing postings and create new ones, enforcing
per-asset conservation. This gives the same safety guarantee as double-entry
bookkeeping (`Σ debits = Σ credits`), expressed as `sum(consumed) == sum(created)`
per asset over signed postings. There are no mutable balance fields; an account's
balance is always the sum of its active postings.

```
┌─────────────────────────────────────────────────────┐
│                   kuatia (async)                    │
│                                                     │
│  Intent layer:  TransferBuilder + commit · balance   │
│  Saga pipeline: resolve → reserve → validate → fin.  │
│  Raw pipeline:  load  →  plan  →  apply             │
│  Saga steps:    legend step adapters                 │
├─────────────────────────────────────────────────────┤
│               kuatia-core (pure)                    │
│                                                     │
│  Types:         Account · Transfer · Posting · Cent │
│  Validation:    validate_and_plan()                 │
│  Hashing:       double-SHA256, content-addressed    │
│  Selection:     greedy posting selection             │
└─────────────────────────────────────────────────────┘
```

## Crates

| Crate | Purpose |
|-------|---------|
| **kuatia-types** | Domain types — `AccountId`, `Posting`, `Transfer`, `Cent`, etc. |
| **kuatia-core** | Pure, sans-IO decision logic — validation, hashing, posting selection. |
| **kuatia-storage** | `Store` trait (7 sub-traits), `InMemoryStore`, `store_tests!` conformance macro. |
| **kuatia-storage-sql** | SQL-backed `Store` — SQLite and PostgreSQL via sqlx. |
| **kuatia** | Async resource layer — `Ledger`, saga commit pipeline, intent-layer API. |

## Quick Example

```rust
use std::sync::Arc;
use kuatia::ledger::Ledger;
use kuatia::mem_store::InMemoryStore;
use kuatia_core::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ledger = Arc::new(Ledger::new(InMemoryStore::new()));

    let usd = AssetId::new(1);
    let alice = AccountId::new(1);
    let bob = AccountId::new(2);
    let bank = AccountId::new(3); // external account

    // Create accounts, deposit, pay...
    // See doc/crates.md for the full API reference.

    Ok(())
}
```

## Documentation

- [Architecture Decisions](doc/architecture.md) — why the ledger works the way it does
- [Crate Reference](doc/crates.md) — modules, types, and APIs per crate
- [Accounts](doc/accounts.md) — account model, policies, and lifecycle
- [Transfers](doc/transfers.md) — Movement struct, resolve algorithm, and TransferBuilder API
- [Glossary](doc/glossary.md) — terms, book scoping, and worked examples
- [Accounting Mapping](doc/accounting-mapping.md) — how classical double-entry concepts map onto kuatia

## License

See [LICENSE](LICENSE) for details.
