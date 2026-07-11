# Immutable postings with active/reserved index tables

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-07-11
* Targeted modules: `kuatia-types` (`Posting`, `PostingState`,
  `PostingFilter`), `kuatia-storage` (`PostingStore`, `InMemoryStore`),
  `kuatia-storage-sql` (schema, migration 004), `kuatia` (`ledger`, `saga`)
* Associated tickets/PRs: N/A

## Context and Problem Statement

ADR-0006 gave every posting a mutable `status` column
(`Active → PendingInactive → Inactive`) plus a nullable `reservation` token,
both flipped in place with `UPDATE`. Consumed postings stayed in the table as
`Inactive` tombstones. That works, but it makes the postings table a
mutate-in-place table: the commit path needs `UPDATE` rights on it, a
historical posting's value or state can be rewritten by any code path (or any
compromised credential) that can issue an `UPDATE`, and every "what can this
account spend" read scans the full postings history filtered by a partial index
on `status`, a set that only grows.

The reservation protocol of ADR-0006 does not actually require the state to live
on the posting. It requires an atomic single-winner claim, durable recoverable
ownership, and count-returning primitives (ADR-0003). Can we keep those
guarantees while making a posting an append-only record that is written once and
never changed?

## Decision Drivers

* **Append-only integrity.** A posting is a fact about a committed transfer.
  Once written it should never change, matching the append-only stance of
  ADR-0001 (value as immutable signed postings) and ADR-0007 (undo by
  compensation, never mutation). The postings table becomes the immutable
  record; the transfer + event logs already record every create and consume.
* **Least privilege / tamper surface.** If postings are insert-only, the role
  that commits transfers needs only `INSERT` on `postings`, never `UPDATE` or
  `DELETE`. No path can rewrite a historical row. Lifecycle churn is confined to
  small index tables that hold ids, not monetary values.
* **Read performance by segregation, not by partial index.** "Spendable" and
  "live" are hot reads. Rather than scan a growing history and filter by
  `status` (a partial index over cold and hot rows mixed together), keep the
  working set in its own physically separate table. The index is the table.
* **Preserve ADR-0006 semantics.** Unconditional, lock-free double-spend safety;
  durable ownership that survives the multi-step saga and a crash; expressible
  as atomic, count-returning dumb-storage instructions (ADR-0003).

## Considered Options

#### Option 1: Keep ADR-0006 (mutable `status` column + partial index)

Leave postings as a mutate-in-place table with `status` + `reservation`.

**Pros:**

* Good, because a posting's state is co-located with its data (one row, no
  join).
* Good, because it is already implemented and conformance-tested.

**Cons:**

* Bad, because the commit path requires `UPDATE` on the value-bearing table, so
  a bug or a compromised credential can rewrite historical postings.
* Bad, because spendable/live reads scan the full, ever-growing history filtered
  by `status`; the hot working set is never physically separated from cold
  tombstones.
* Bad, because the postings table is not append-only, at odds with ADR-0001/0007.

#### Option 2: Single active table, reservation only in the write-ahead record

Delete a posting id from a single `active_postings` table to reserve it; keep
the reservation solely in the saga's write-ahead `PendingSaga` blob (ADR-0003).

**Pros:**

* Good, because the schema is minimal (one index table).

**Cons:**

* Bad, because reserved (in-flight) postings are no longer observable in storage,
  so balance (`Active + Reserved`) and `close` (blocks on live) change behavior,
  and recovery cannot read "reserved by rid" from a row.
* Bad, because it silently alters the observable semantics ADR-0006 fixed.

#### Option 3: Immutable postings + two id-only index tables

`postings` becomes insert-only and immutable:
`(transfer_id, idx, owner, subaccount, asset, value)`, no `status`, no
`reservation`. Two index tables hold only ids: `active_postings` (membership =
spendable) and `reserved_postings` (`(id, reservation)`, membership = claimed by
a saga). A posting's state is derived from membership: in active → `Active`, in
reserved → `Reserved(rid)`, in neither → `Spent`. Every transition is an
insert/delete on an index table; the posting row never changes:

* Create (finalize): `INSERT` into `postings`, then `INSERT` id into
  `active_postings`.
* Reserve: `DELETE` id from `active_postings`; if it removed a row, `INSERT`
  id + rid into `reserved_postings`. The delete-returns-one is the atomic
  single-winner claim.
* Consume (finalize): `DELETE` id from `reserved_postings` where reservation =
  rid. The posting stays in `postings` forever, now in neither index = spent.
* Release (compensation): `DELETE` from `reserved_postings`, `INSERT` back into
  `active_postings`.

**Pros:**

* Good, because `postings` is append-only. The commit role needs only `INSERT`
  on it; no code path or credential can rewrite a historical posting. The
  immutable table is the audit trail, with history also reconstructable from the
  transfer + event logs.
* Good, because the active and reserved working sets are physically separate,
  small, and id-only. Spendable/live reads hit a dedicated table instead of
  scanning history behind a partial index.
* Good, because reservation is still a single atomic claim (the delete-CAS picks
  one winner; the loser sees zero rows), and ownership is still durable and
  observable: `reserved_postings.reservation` records who holds a posting, so
  recovery and finalize target only their own rows exactly as in ADR-0006.
* Good, because it stays within dumb storage: each primitive is one conditional
  insert/delete returning an affected-row count; the saga interprets it
  (ADR-0003).
* Good, because balance (`Active ∪ Reserved`) and `close` (blocks on any live)
  keep their exact prior semantics, now expressed as index membership instead of
  a `status` filter.

**Cons:**

* Bad, because a posting's state is no longer co-located with its data: reading
  state is a membership probe across two tables (`get_posting_states`), and
  three tables replace one.
* Bad, because consuming an already-spent posting no longer surfaces as a
  plan-time validation error; a spent posting is simply absent from the indexes,
  so the abort moves to the reserve claim / finalize guard (same safety,
  different surface).
* Bad, because the schema change is forward-only (migration 004 rebuilds
  `postings` without the `status`/`reservation` columns).

## Decision Outcome

Chosen option: **Option 3, immutable postings with active/reserved index
tables**, because it keeps every guarantee of ADR-0006 (lock-free double-spend
safety, durable recoverable ownership, count-returning primitives) while making
the value-bearing table append-only. That buys least-privilege security (the
commit role needs no `UPDATE` on postings, so historical rows are
tamper-evident by construction) and read performance by segregation (the hot
active/reserved working set lives in its own id-only tables rather than behind a
partial index over ever-growing history).

This supersedes the storage representation of ADR-0006. The reservation protocol
of ADR-0006 stands; only its physical encoding changes, from an in-place
`status` transition to index-table membership.

### Positive Consequences

* `postings` is insert-only: grant `INSERT`, withhold `UPDATE`/`DELETE`.
* Reserve is `DELETE FROM active_postings` as the concurrency gate; the saga
  reads the affected-row count to know it won (ADR-0003).
* Recovery still distinguishes "reserved by this saga" (row present in
  `reserved_postings` with our rid) from "spent" (absent from both indexes) and
  "taken by another" (reserved by a different rid), which is what makes
  phase-tracked roll-forward safe.
* The immutable postings table and the append-only logs can be archived or
  pruned independently of the live working set, which partly addresses the
  deferred retention question (README "Recommended future ADRs").

### Negative Consequences

* Reading a posting's state is a probe across `active_postings` /
  `reserved_postings` / `postings` rather than one column.
* Three tables instead of one; ports of the store must keep the reserve claim
  (delete-from-active + insert-into-reserved) atomic.
* No `Inactive` tombstone: a spent posting's state is "present in `postings`,
  absent from the indexes." Consumers that read per-posting state use the
  derived `PostingState`, not a stored column.

## Links

* Supersedes the storage representation of
  [ADR-0006](0006-reservation-protocol-posting-lifecycle.md) (the reservation
  protocol itself is unchanged).
* Builds on [ADR-0001](0001-modified-utxo-signed-postings.md) (immutable signed
  postings) and the dumb-storage recovery of
  [ADR-0003](0003-dumb-storage-saga-recovery.md).
* Extended by the conformance suite of
  [ADR-0008](0008-conformance-tested-storage.md) (membership-based transition
  tests).
* Background: [architecture.md](../architecture.md) ("Posting Lifecycle").
