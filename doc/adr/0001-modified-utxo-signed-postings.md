# Modified UTXO: value as signed postings, not mutable balances

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-06-29
* Targeted modules: `kuatia-types`, `kuatia-core`, `kuatia-storage`
* Associated tickets/PRs: N/A

## Context and Problem Statement

A ledger must record value movements so that the result is auditable,
supports multiple assets, behaves well under concurrency, and can
represent overdraft or credit lines. How should value be represented at
rest? The naïve answer, a mutable balance field per `(account, asset)`,
throws away history and turns every account into a write-contended hot
row. We want double-entry *safety* (`Σ debits = Σ credits`) without the
brittleness of mutable running totals.

## Decision Drivers

* **Auditability**: the full financial state should be reconstructible
  by replaying recorded events, with no "trust me" mutable totals.
* **Multi-asset**: one model for many assets, not a balance column per
  asset.
* **Concurrency**: avoid a single hot balance row that serializes all
  activity on an account.
* **Conservation as a structural property**: make "nothing is created
  or destroyed" a checkable invariant, not a convention.
* **Overdraft / credit**: represent a negative position naturally,
  bounded by an account policy.
* **Embeddability**: pure, deterministic core logic with no database
  arithmetic.

## Considered Options

#### Option 1: Mutable balance fields per `(account, asset)`

A row holds the current balance; each transfer reads, mutates, and
writes it.

**Pros:**

* Good, because balance reads are O(1): just read the column.
* Good, because it is the most familiar model.

**Cons:**

* Bad, because history is lost: you cannot audit how a balance was
  reached without a separate, independently-trusted journal.
* Bad, because the balance row is a concurrency hot spot; every transfer
  on an account contends on it.
* Bad, because conservation is a convention enforced by application
  code, not a property of the data.

#### Option 2: Classic (Bitcoin-style) UTXO, non-negative outputs

Value is held as immutable outputs that are consumed and created;
outputs cannot be negative.

**Pros:**

* Good, because it is auditable (consume/create history) and avoids a
  hot row.
* Good, because conservation is naturally expressible
  (`sum in == sum out`).

**Cons:**

* Bad, because non-negative outputs cannot represent overdraft or credit
  lines.
* Bad, because the Bitcoin model carries scripting/locking machinery
  irrelevant to an accounting ledger.

#### Option 3: Signed postings ("modified UTXO")

Value is held as **signed postings**: an immutable, signed amount of one
asset owned by one account, with a lifecycle
(`Active → PendingInactive → Inactive`). A transfer **consumes**
postings and **creates** postings; a posting may be **negative** (an
"offset position") to represent an overdraft.

**Pros:**

* Good, because it keeps full history and is auditable by replaying the
  transfer log; balances are *projections* over `Active` postings and
  are never stored.
* Good, because conservation is enforced directly:
  `sum(consumed) == sum(created)` per asset on every committed transfer
  (`validate_and_plan`, `ConservationViolation`).
* Good, because it is multi-asset by construction (a posting names its
  asset).
* Good, because there is no hot balance row: different postings of the
  same account can be touched independently, enabling UTXO-style
  concurrency and future sharding.
* Good, because overdraft is just a negative posting, bounded by account
  policy (`NoOverdraft`, `CappedOverdraft { floor }`,
  `UncappedOverdraft`, and so on).

**Cons:**

* Bad, because a balance is computed (sum over `Active` postings), not
  read: a read cost the store must support efficiently.
* Bad, because the word *posting* (a noun: a value fragment) collides
  with the accounting verb "to post", a documented source of confusion.
* Bad, because spending requires **posting selection** and change-making
  (greedy largest-first), machinery a mutable-balance model would not
  need.

## Decision Outcome

Chosen option: **Option 3, signed postings ("modified UTXO")**, because
it is the only option that delivers auditability, a hot-row-free
concurrency model, and natural overdraft, while making per-asset
conservation a structural, checkable invariant. The "modification" over
classic UTXO is precisely that postings may be negative (offset
positions) and there is no scripting layer.

### Positive Consequences

* The transfer log (`TransferStore` of `EnvelopeRecord`s) is the
  append-only source of truth; balances are derived, so they cannot
  silently diverge.
* Double-entry safety is enforced in the pure core and testable with
  golden vectors, independent of any storage backend.
* The posting lifecycle (Active/PendingInactive/Inactive) gives the saga
  a place to reserve inputs without mutating balances. See ADR-0002 and
  ADR-0003.

### Negative Consequences

* Balance queries sum over `Active` postings; the store indexes
  `(owner, asset, status)` to keep this cheap, and all such arithmetic
  is done in Rust with checked operations (never in SQL).
* Posting selection / change-making is required on the spend path.
* Terminology must be taught: see
  [accounting-mapping.md](../accounting-mapping.md) for the
  classical-accounting to Kuatia mapping and the noun/verb caveat.

## Links

* Refined by [ADR-0002](0002-saga-commit-pipeline.md) (how postings are
  committed) and [ADR-0003](0003-dumb-storage-saga-recovery.md) (the
  storage/commit contract).
* Background: [accounting-mapping.md](../accounting-mapping.md),
  [glossary.md](../glossary.md), [accounts.md](../accounts.md).
