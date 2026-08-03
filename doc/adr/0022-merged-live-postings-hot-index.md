# Merge the active/reserved hot indexes into one `live_postings` table

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-08-01
* Targeted modules: `kuatia-storage-sql` (schema, migration 008,
  `PostingStore`)
* Associated tickets/PRs: N/A

## Context and Problem Statement

ADR-0016/0017 split a posting's live state across two disposable hot tables:
`active_postings` (membership = spendable) and `reserved_postings` (membership +
`reservation` = claimed by a saga). A "live" read (`Active ∪ Reserved`), which
`compute_balance` and `has_live_postings` need, therefore has no single table to
hit: the SQL backend expresses it as a `UNION ALL` of the two tables. Reserve and
release move a row between the two tables (`DELETE` from one, `INSERT` into the
other), and `get_posting_states` probes three tables.

Only the SQL encoding forces the `UNION ALL`; the two-table split is not required
by any correctness guarantee (ADR-0006's reservation protocol needs an atomic
single-winner claim, durable observable ownership, and count-returning
primitives, none of which depend on the physical table count). Can the live set
live in one table so the `UNION ALL` disappears, without losing those
guarantees?

## Decision Drivers

* **Query clarity and read performance.** The live/spendable reads are hot
  (`compute_balance` runs per commit in the finalize validation loader). A
  single-table scan is simpler to reason about and to index than a `UNION ALL`.
* **Preserve every ADR-0006/0016 guarantee.** Unconditional lock-free
  double-spend safety; durable, observable, recoverable reservation ownership;
  atomic count-returning dumb-storage primitives (ADR-0003); balance as
  `Active ∪ Reserved`; append-only value table.
* **Keep the source of truth append-only (ADR-0017).** `postings` (the immutable
  record) must stay INSERT-only and rebuildable-from is the audit trail. Whatever
  changes may only touch a disposable hot table.

## Considered Options

#### Option 1: Keep two hot tables (ADR-0016/0017)

`active_postings` + `reserved_postings`, live reads via `UNION ALL`, reserve as
`DELETE`-from-active + `INSERT`-into-reserved.

**Pros:**

* Good, because the entire hot-table write path is `INSERT` + `DELETE`, so a
  grant can withhold `UPDATE` on them too.
* Good, because it is already implemented and conformance-tested.

**Cons:**

* Bad, because a live read has no single table and is a `UNION ALL`.
* Bad, because reserve/release are two-statement moves and `get_posting_states`
  probes three tables.

#### Option 2: One `live_postings` table with a nullable `reservation`

Merge the two hot tables into one: `live_postings (transfer_id, idx, owner,
subaccount, asset, value, reservation)`, `reservation` NULL = Active, set =
Reserved by that id. State is still derived from membership + the column: present
& NULL = Active; present & = rid = Reserved(rid); absent from `live_postings` but
in `postings` = Spent; absent everywhere = Missing. The primitives become:

* Live read: `SELECT ... FROM live_postings WHERE owner/subaccount/asset` (no
  `UNION`). Active = `... AND reservation IS NULL`, Reserved = `... AND
  reservation IS NOT NULL`.
* Reserve: `UPDATE live_postings SET reservation = $rid WHERE <ids> AND
  reservation IS NULL`. The `IS NULL` guard is the atomic single-winner claim
  (concurrent reserves serialize on the row lock; the loser's predicate no longer
  matches → 0 rows). One statement.
* Release: `UPDATE live_postings SET reservation = NULL WHERE <ids> AND
  reservation = $rid`.
* Consume (finalize) / raw deactivate: `DELETE FROM live_postings WHERE <ids> AND
  reservation = $rid` (or `IS NULL`); the posting stays in `postings` = Spent.
* `get_posting_states`: two statements (live table + `postings`) instead of three.

**Pros:**

* Good, because the live set is one table: the `UNION ALL` is gone, reserve and
  release are single statements, and `get_posting_states` drops a statement.
* Good, because every ADR-0006 guarantee holds: the `UPDATE ... WHERE reservation
  IS NULL` gives the same single-winner claim as the `DELETE`-CAS; the
  `reservation` column is still the durable, observable ownership recovery reads;
  the primitives still return affected-row counts the saga interprets.
* Good, because the value table `postings` stays append-only INSERT-only, and
  `live_postings` is still fully rebuildable from `postings` plus the saga
  write-ahead records, so a corrupt hot table is drop-and-rebuild (ADR-0017's
  disposability principle is preserved).

**Cons:**

* Bad, because the hot table now takes an `UPDATE` (the reservation flip), so the
  "hot tables are `INSERT`/`DELETE` only, withhold `UPDATE` everywhere" grant
  story of ADR-0017 no longer holds for `live_postings` (it still holds for the
  value tables).
* Bad, because the schema change is forward-only (migration 008 merges the two
  tables and drops them).

#### Option 3: Keep the `UNION ALL` but hide it behind a view

A SQL `VIEW live_postings AS active UNION ALL reserved`.

**Pros:**

* Good, because callers read one name.

**Cons:**

* Bad, because it is the same `UNION ALL` at runtime; nothing is actually saved.

## Decision Outcome

Chosen option: **Option 2, one `live_postings` table with a nullable
`reservation`.** It removes the `UNION ALL` and collapses reserve/release to a
single statement while preserving every correctness guarantee of ADR-0006/0016.
The one concession is that the disposable hot table now takes an `UPDATE` (the
reservation flip): this is deliberately confined to the rebuildable hot table and
never touches the append-only `postings` value table, so ADR-0017's core driver
(the source of truth cannot be corrupted or lost by any write path) is intact.

This supersedes the two-table hot-index *encoding* of ADR-0016 and ADR-0017. Their
principle (append-only value tables plus disposable hot indexes) stands; only the
number of hot tables and the reserve verb change.

### Positive Consequences

* A live/spendable read is one index scan on `idx_live_owner`, no `UNION ALL`.
* Reserve and release are single `UPDATE` statements; `get_posting_states` is two
  queries instead of three.
* `reservation IS NULL` vs `= rid` still expresses Active vs Reserved, so state is
  still derived, and recovery still reads "reserved by this saga" from the column.

### Negative Consequences

* The `live_postings` hot table takes an `UPDATE`; the grant that withholds
  `UPDATE` now applies to the value tables only, not the hot table.
* Forward-only migration (008) that drops `active_postings` / `reserved_postings`.

## Links

* Supersedes the two-table hot-index encoding of
  [ADR-0016](0016-immutable-postings-index-tables.md) and
  [ADR-0017](0017-correctness-first-append-only-hot-indexes.md); their
  append-only-value / disposable-hot-index principle is unchanged.
* Preserves the reservation protocol of
  [ADR-0006](0006-reservation-protocol-posting-lifecycle.md) and the dumb-storage
  primitives of [ADR-0003](0003-dumb-storage-saga-recovery.md).
* Reflected in [storage-schema.md](../storage-schema.md).
