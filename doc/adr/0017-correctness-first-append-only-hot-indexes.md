# Correctness-first storage: append-only value tables, disposable hot indexes

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-07-11
* Targeted modules: `kuatia-storage`, `kuatia-storage-sql`, and the storage
  schema (postings, accounts, and their hot tables)
* Associated tickets/PRs: N/A

## Context and Problem Statement

The ledger's correctness rests on never losing or corrupting value and audit
data. Earlier decisions evolved the storage piecemeal: value as immutable signed
postings (ADR-0001), the reservation lifecycle (ADR-0006), and moving a posting's
lifecycle state out of a mutable column into separate tables (ADR-0016).
Accounts followed the same append-only shape, and the separate tables later
gained full row copies for direct reads.

This ADR states the principle those changes converged on, so the *why* is
explicit rather than implied across three ADRs: how should the schema be shaped
so that the correctness of the ledger does not depend on trusting every write
path to touch mutable state correctly?

## Decision Drivers

* **Correctness (primary).** The source of truth must not be corruptible or
  loseable by a bug or a mistaken write. A mistake in a fast-access structure
  should be recoverable, not a data-loss event.
* **Least privilege / defense in depth.** The database itself should be able to
  reject the dangerous operations, not just the application. This is only
  possible if the write patterns map to a small, grantable set of operations.
* **Read performance.** The common "what is spendable / what is the current
  account" reads should hit a small, dedicated structure, not scan history.

## Considered Options

#### Option 1: Mutable-in-place columns

Keep one table per entity and mutate a `status` / current-version column in
place with `UPDATE`.

**Pros:**

* Good, because state is co-located with the row (one table, no duplication).

**Cons:**

* Bad, because the value and audit data lives in a table that is updated in
  place, so a bug or a compromised credential can silently rewrite or lose
  history. Correctness depends entirely on every write path being correct.
* Bad, because the commit role must hold `UPDATE` on the value-bearing table,
  which cannot then be withheld as a safety net.

#### Option 2: One table plus a partial index on the mutable column

Keep the single mutable table but add a partial index over the "live" rows.

**Pros:**

* Good, because hot reads can use the partial index.

**Cons:**

* Bad, because the table is still mutated in place (same correctness and
  privilege problems as Option 1); the hot and cold rows still share one
  ever-growing, mutable table.

#### Option 3: Append-only value tables plus disposable hot indexes

Split every entity into two kinds of table:

* An **append-only value / audit table** that is only ever inserted into and
  never updated or deleted. This is the immutable source of truth: `postings`
  (every posting ever created), `accounts` (every account version).
* Zero or more **disposable hot tables** that index the current / live subset for
  fast access, maintained only by `INSERT` and `DELETE` (never `UPDATE`), and
  fully rebuildable from the value tables: `active_postings` and
  `reserved_postings` (the spendable / in-flight set), `account_head` (each
  account's current version).

Derived state (a posting's lifecycle, an account's current version) is read from
hot-table membership, not from a mutable column. The hot tables carry full row
copies of the live set, so reads hit them directly without joining back to the
value tables.

**Pros:**

* Good, because the source of truth is append-only and therefore cannot be
  corrupted or lost by any write path. Correctness no longer depends on trusting
  updates to be right.
* Good, because a bug in a disposable hot table is recoverable: the hot tables
  are derivable from the value tables and can be dropped and rebuilt.
* Good, because the entire write path reduces to `INSERT` and `DELETE`, which the
  database can enforce with grants: the ledger role gets `INSERT` on the value
  tables, `INSERT` + `DELETE` on the hot tables, and `UPDATE` on nothing. The
  margin for error is enforced below the application.
* Good, because the hot tables are small and directly readable, so the common
  reads are fast.

**Cons:**

* Bad, because live rows are duplicated between a value table and its hot table.
* Bad, because the write primitives must keep the hot tables consistent with the
  value tables (reserve/release/consume move rows between hot tables).
* Bad, because per-row state is a membership probe across tables rather than a
  single column read.

## Decision Outcome

Chosen option: **Option 3, append-only value tables plus disposable hot
indexes.** Correctness is the deciding driver: making the source of truth
append-only means no write can corrupt or lose it, and confining all mutation to
`INSERT` and `DELETE` lets the database enforce that guarantee through grants
rather than trusting the application. The performance and least-privilege
benefits follow from the same shape.

The rule going forward: value and audit data lives only in append-only tables;
anything mutable is a disposable hot table that indexes the current or live
subset, is maintained by `INSERT` and `DELETE` only, and can be rebuilt from the
value tables.

### Positive Consequences

* The source of truth (`postings`, `accounts`) is append-only and tamper-evident
  by construction.
* The write path is grantable as `INSERT` + `DELETE` with no `UPDATE` anywhere,
  so the database rejects the dangerous operation independently of the code.
* Hot tables are disposable and rebuildable, so a defect there is recoverable
  rather than a loss of truth.
* Hot reads hit small, dedicated, directly-readable tables.

### Negative Consequences

* Live rows are stored twice (value table + hot table).
* The move primitives (reserve/release/consume) own keeping the hot tables
  consistent with the value tables.
* Reading a single entity's state is a membership probe, not one column.

## Links

* Generalizes and refines
  [ADR-0016](0016-immutable-postings-index-tables.md), and supersedes its
  "id-only" hot-table detail: the hot tables now carry full row copies so reads
  do not merge back to the value table.
* Builds on [ADR-0001](0001-modified-utxo-signed-postings.md) (value as immutable
  signed postings) and [ADR-0006](0006-reservation-protocol-posting-lifecycle.md)
  (the reservation lifecycle the hot tables encode).
* Covers `account_head` (the current-version hot table for accounts) and the
  set-based reserve/release/consume primitives that move rows between hot tables.
