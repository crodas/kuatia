# Saga commit pipeline instead of a single/distributed transaction

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-06-29
* Targeted modules: `kuatia` (`ledger`, `saga`), `kuatia-storage`
* Associated tickets/PRs: N/A

## Context and Problem Statement

Committing a transfer is not one write. It must consume input postings, create
output postings, persist the transfer record, index it under every involved
account, and emit events, consistently and recoverably if the process crashes
mid-way. Kuatia is an embeddable library that should not require an external
transaction coordinator, and it should compose multi-transfer workflows (e.g.
an FX trade, a multi-leg settlement) without a global lock. How do we make a
commit both consistent and crash-safe without a single all-encompassing
transaction?

## Decision Drivers

* **Crash-safety**: a crash must never leave value created without a recorded
  transfer (or vice-versa); recovery must converge.
* **Composability**: several transfers should combine into one logical workflow
  with rollback across the whole workflow.
* **No external coordinator**: embeddable, no XA transaction manager or
  separate service to operate.
* **Avoid cross-shard / cross-resource transactions**: the model should not
  depend on a single database transaction spanning everything, so future
  sharding (UTXO-style) stays open.
* **Keep storage dumb**: push domain decisions out of the storage layer (see
  ADR-0003), which a monolithic store-side transaction works against.

## Considered Options

#### Option 1: One database transaction (monolithic commit)

Do everything (deactivate, insert, store record, index, events) inside a single
`BEGIN … COMMIT`.

**Pros:**

* Good, because it gives strict atomicity on a single database.
* Good, because recovery is trivial: the DB rolls back a partial commit.

**Cons:**

* Bad, because it does not compose across multiple transfers/resources: a
  multi-leg workflow cannot be one DB transaction without holding locks across
  all of it.
* Bad, because it does not span shards/resources; it pins the design to a
  single transactional store.
* Bad, because it pulls domain logic (guards, ownership, indexing decisions)
  into the storage layer, the opposite of the dumb-storage goal (ADR-0003).

#### Option 2: Two-phase commit / XA across resources

Coordinate a distributed transaction over the resources involved.

**Pros:**

* Good, because it offers cross-resource atomicity.

**Cons:**

* Bad, because it needs a coordinator and XA-capable resources: heavy
  operationally and at odds with "embeddable, no external services."
* Bad, because 2PC is blocking: a coordinator failure can leave resources
  locked.
* Bad, because it is overkill for a library meant to drop into an application.

#### Option 3: Saga with per-step compensation (the `legend` crate)

Model the commit as a pipeline of small steps (reserve → finalize), each with
a compensating action, driven by `legend`, with automatic retry and LIFO
compensation. Crash-safety comes from a write-ahead record plus idempotent
recovery rather than a global transaction.

**Pros:**

* Good, because steps compose: higher-level multi-transfer sagas combine the
  same primitives with workflow-wide compensation.
* Good, because no global or distributed transaction and no coordinator are
  required; it suits an embeddable library.
* Good, because reserving inputs (`Active → PendingInactive`, stamped with a
  `ReservationId`) gives unconditional double-spend safety without locking
  balances: a single atomic conditional update per posting.
* Good, because it keeps storage dumb: each step issues simple instructions and
  the saga owns the decisions (ADR-0003).

**Cons:**

* Bad, because a commit passes through brief intermediate states (postings
  reserved/partially finalized) rather than flipping atomically.
* Bad, because idempotency and end-state verification become the saga's
  responsibility, not the database's.
* Bad, because some cross-cutting guarantees (e.g. the CappedOverdraft floor)
  can only be made best-effort without a commit-time atomic guard. See
  ADR-0003.

## Decision Outcome

Chosen option: **Option 3, a saga commit pipeline** (`reserve → finalize`,
via `legend`), because it is the only option that composes multi-transfer
workflows, needs no external coordinator, and keeps storage dumb, while still
providing crash-safety (through a write-ahead record + idempotent recovery)
and unconditional double-spend safety (through the reservation protocol).

### Positive Consequences

* The same steps drive both a single commit and multi-transfer workflows, with
  automatic retry and LIFO compensation on logical failure.
* Reservation makes consumed-posting double-spend impossible without a global
  lock, preserving the UTXO concurrency model from ADR-0001.
* Storage stays a thin record keeper; the commit's correctness lives in one
  testable place.

### Negative Consequences

* Crash-safety is not "the DB rolls it back" but write-ahead + roll-forward
  recovery, which the saga must implement idempotently. See ADR-0003.
* Within a commit there are observable intermediate posting states.
* The exact recovery model and the best-effort guards are non-trivial and are
  the subject of ADR-0003.

## Links

* Refined by [ADR-0003](0003-dumb-storage-saga-recovery.md): the storage
  contract, count interpretation, and the durable phase-tracked recovery the
  saga relies on.
* Builds on [ADR-0001](0001-modified-utxo-signed-postings.md): the posting
  lifecycle the reserve step uses.
* Background: [architecture.md](../architecture.md),
  [transfers.md](../transfers.md).
