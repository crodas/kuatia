# Atomic storage commit, retiring the write-ahead saga and `legend`

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-08-12
* Targeted modules: `kuatia-storage`, `kuatia-storage-sql`,
  `kuatia` (`ledger`, `saga`)
* Associated tickets/PRs: N/A

## Context and Problem Statement

ADR-0003 made storage a set of dumb single-update primitives and moved
crash-safety into a phase-tracked write-ahead record (`PendingSaga` /
`PendingTransition`) plus roll-forward recovery (`Ledger::recover`). A commit was
a sequence of primitives (`reserve → deactivate → insert → store_transfer →
append_event`) with no shared transaction, so the write-ahead log was the only
thing protecting against a half-applied commit. That design also left the
overdraft floor and the freeze/close guards **best-effort** under concurrency,
re-checked just before the writes but not atomic with them.

Two things prompted revisiting it. The `legend` saga VM had already been reduced
to two thin adapters used only by tests: the single-commit path was a linear
method, not a saga. And the whole write-ahead machinery existed only because the
store could not commit a transfer atomically. If it can, the reservation
two-step, the write-ahead record, and recovery all become unnecessary.

Should the store apply a whole transfer atomically, reversing ADR-0003's dumb
per-primitive model?

## Decision Drivers

* **Simplicity where it is safe.** Kuatia targets a single transactional
  database (SQLite or PostgreSQL) or an in-process store. On each, a whole-commit
  transaction is available and is the simplest correct primitive.
* **Strict guards.** The overdraft floor and freeze/close checks should be
  enforced atomically with the write, not best-effort.
* **No orphaned state.** An all-or-nothing commit leaves nothing for recovery to
  reconcile.
* **One place for correctness.** Validation stays in the pure core; the store
  enforces the stateful guards inside its transaction. There is no third home
  (the saga's count-contract) to keep in sync.

## Considered Options

#### Option 1: Keep ADR-0003 (dumb primitives + write-ahead recovery)

**Pros:**

* Composes across multiple resources/shards without a database transaction.

**Cons:**

* The write-ahead record, phase tracking, and roll-forward recovery are a large
  surface that exists solely to stitch non-atomic primitives together.
* The floor and freeze/close guards stay best-effort under concurrency.

#### Option 2: Atomic storage commit (chosen)

Add one `CommitStore` sub-trait with `commit_envelope` and `commit_transition`,
each applied in a single store transaction (SQL: one `BEGIN..COMMIT`; in-memory:
all relevant locks held across the apply). The pure core still validates first;
the transaction re-checks the three stateful guards inside it: double-spend
(the delete-affected-count is the atomic single-winner claim), freeze/close, and
the overdraft floor (summed in Rust from the live rows, never in SQL).

**Pros:**

* Deletes the reservation two-step, `PendingSaga`/`PendingTransition`,
  `Ledger::recover`, and the `SagaStore` keyspace. A crash cannot half-apply a
  commit, so there is nothing to recover.
* The floor and freeze/close guards become strict, removing the ADR-0003
  best-effort consequence.
* `legend` and the count-contract helpers are removed; multi-transfer
  composition is a plain LIFO runner (`run_movements`) over `commit` / `reverse`.

**Cons:**

* Crash-safety is pinned to a per-backend transaction: it does not compose across
  shards or multiple databases. This forecloses the cross-shard UTXO ambition
  ADR-0002/0003 kept open, which is an accepted trade for a single-database
  library.
* On SQLite, `pool.begin()` is a deferred transaction; the delete-affected-count
  remains the double-spend backstop, the same property the prior design relied
  on.

## Decision Outcome

Chosen: **Option 2.** The store owns the atomic commit boundary; the reservation
index, write-ahead recovery, `SagaStore`, and `legend` are removed. This
supersedes ADR-0003 and refines ADR-0002 (there is no longer a saga pipeline for
a single commit). ADR-0020's transition recovery is subsumed by the atomic
`commit_transition`, and ADR-0021's commit-safety map moves the double-spend,
floor, and freeze/close guards into `commit_envelope`.

### Positive Consequences

* Fewer moving parts: no write-ahead log, no recovery, no saga VM.
* Strict floor and freeze/close guards.
* One validation home (pure core) plus one enforcement home (the store
  transaction).

### Negative Consequences

* No cross-shard / multi-resource commit. If that is ever needed, it is a new
  design, not a tweak to this one (see the next section for where it would live).
* The generic `reserve_postings` / `release_postings` posting primitives are now
  unused by the commit path; they remain as conformance-tested plumbing and can
  be removed in a follow-up.

## When a saga is the right tool (and why not here)

Removing the saga is not a verdict against the pattern. A saga earns its keep
when a workflow spans **moving pieces that cannot share one transaction**:
multiple services, multiple databases, or a sharded store where a single commit
cannot reach every partition. There it sequences local steps and compensates the
completed ones on failure, buying atomicity the underlying resources cannot
provide on their own, and it pays for that with write-ahead records, phase
tracking, and compensation logic.

Kuatia was not that case. Every primitive the commit saga orchestrated
(`reserve`, `deactivate`, `insert`, `store_transfer`, `append_event`) lived in
**one** store behind **one** transactional boundary. The saga was coordinating
pieces that were already coupled, so it reimplemented in application code the
atomicity the database already provides for free. When a single ACID transaction
is available, that transaction is the simpler and stricter primitive, and the
saga is ceremony around it.

The escape hatch stays open, but it belongs one layer down. If a storage backend
ever shards its data so that a transfer's postings span partitions no single
transaction can cover, **that backend** implements
`CommitStore::commit_envelope` with an internal saga (two-phase commit, per-shard
local transactions plus compensation, or similar). The `CommitStore` contract,
apply the whole envelope atomically or reject it, says nothing about *how* a
backend achieves atomicity. The saga would then live where the moving pieces
actually are, and the ledger core stays oblivious: it still just calls
`commit_envelope` and maps the outcome. The pattern moves to the layer that has
the distribution problem, instead of sitting in the core that does not.
