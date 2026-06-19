# Reservation protocol and the posting lifecycle

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-06-29
* Targeted modules: `kuatia-types` (`PostingStatus`, `ReservationId`),
  `kuatia-storage`, `kuatia` (`saga`)
* Associated tickets/PRs: N/A

## Context and Problem Statement

A commit must consume input postings exactly once, even when many commits run
concurrently and the commit itself is a multi-step saga (ADR-0002) over a dumb
store (ADR-0003) with no global transaction. Two sagas must never both spend
the same posting, and a posting reserved by one saga must not be finalized or
released by another. How is exclusive, recoverable ownership of inputs achieved
without locking account balances?

## Decision Drivers

* **Double-spend safety:** a posting can be consumed at most once,
  unconditionally.
* **No hot-row locking:** preserve the UTXO concurrency of ADR-0001; do not
  serialize on an account balance.
* **Survives the saga's lifetime:** a reservation must hold across reserve →
  validate → finalize and across a crash + recovery.
* **Ownership:** only the saga that reserved a posting may finalize or release
  it.
* **Fits dumb storage:** expressible as single atomic conditional updates that
  return affected-row counts (ADR-0003).

## Considered Options

#### Option 1: Database row locks (`SELECT … FOR UPDATE`) per posting

Lock the posting rows for the duration of the commit.

**Pros:**

* Good, because it gives strict mutual exclusion within one database
  transaction.

**Cons:**

* Bad, because it requires a transaction spanning the whole commit, which the
  saga model deliberately avoids (ADR-0002/0003).
* Bad, because held locks block other workers and do not survive a crash
  (the lock is gone, but no record says the posting was claimed).
* Bad, because it ties the design to a locking, transactional store.

#### Option 2: Optimistic balance CAS per account

Guard each commit with a compare-and-set on the account balance.

**Pros:**

* Good, because it avoids long-held locks.

**Cons:**

* Bad, because it serializes on a per-account balance (the hot row ADR-0001 set
  out to avoid) and conflates "did this posting get spent" with "did the
  balance change."
* Bad, because it does not, by itself, record exclusive ownership of specific
  inputs for recovery.

#### Option 3: A three-state posting lifecycle with a reservation token

A posting is `Active → PendingInactive → Inactive`.
`reserve_postings(ids, rid)` flips `Active → PendingInactive` and stamps each
with a `ReservationId`, as a single atomic conditional update
(`… WHERE status = Active`). `release_postings` reverts
`PendingInactive → Active` for the owning `rid`; finalize flips
`PendingInactive (owned by rid) → Inactive`. The reservation id is durable
(persisted with the write-ahead record, ADR-0003), and every later mutation is
conditioned on ownership.

**Pros:**

* Good, because reservation is a single atomic conditional update. Two sagas
  cannot both move the same `Active` posting to `PendingInactive`; the loser
  sees zero rows affected. Double-spend safety is unconditional and lock-free.
* Good, because the `PendingInactive` state plus `ReservationId` is durable
  ownership that survives the multi-step saga and a crash, enabling recovery to
  tell "reserved by us" from "taken by someone else."
* Good, because it expresses cleanly over dumb storage (counts, not locks) and
  keeps balances out of the critical section.
* Good, because compensation is natural: release reverts the reservation.

**Cons:**

* Bad, because a posting carries lifecycle state and an optional reservation
  column (more than an immutable UTXO).
* Bad, because a reservation orphaned by a crash must be resolved by recovery
  (roll-forward) rather than by a lock simply being dropped. ADR-0003 handles
  this.

## Decision Outcome

Chosen option: **Option 3: the three-state posting lifecycle with a durable
`ReservationId`**, because it is the only option that gives unconditional,
lock-free double-spend safety and durable, recoverable ownership across a
multi-step saga, while expressing as the atomic, count-returning instructions
the dumb store provides.

### Positive Consequences

* `reserve_postings` is the concurrency gate; the saga reads its affected-row
  count to know it won the reservation (ADR-0003's count interpretation).
* Recovery distinguishes "reserved by this saga" / "already finalized by us"
  from "taken by another transfer," which is what makes phase-tracked
  roll-forward safe (ADR-0003).
* Balances never enter the critical section. UTXO concurrency is preserved.

### Negative Consequences

* Postings are not pure immutable UTXOs; they carry `status` + `reservation`.
* Crash-orphaned reservations are resolved by recovery, not by lock release.

## Links

* The primitive behind [ADR-0002](0002-saga-commit-pipeline.md) and the
  recovery in [ADR-0003](0003-dumb-storage-saga-recovery.md).
* Builds on [ADR-0001](0001-modified-utxo-signed-postings.md).
* Background: [architecture.md](../architecture.md) ("Posting Lifecycle"),
  [glossary.md](../glossary.md) ("Reservation protocol").
