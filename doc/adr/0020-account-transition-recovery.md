# Crash-safe account-version transitions

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-07-18
* Targeted modules: `kuatia` (`ledger`), `kuatia-storage`, `kuatia-storage-sql`
* Associated tickets/PRs: N/A

## Context and Problem Statement

ADR-0003 made the commit path crash-safe with a phase-tracked write-ahead
record and a roll-forward `recover()`. Account lifecycle transitions
(`freeze`, `unfreeze`, `close`) never got the same treatment. Each was an
independent copy of the same shape: load the account, reject if closed, append
a new account version with the flag flipped, then append the lifecycle event.
The version append and the event append are two separate store writes with no
shared transaction, so a crash between them left a durable version bump with no
event, and nothing repaired it. That is the exact window `recover()` closes for
`store_transfer → append_event` on the commit path, left open for account
transitions.

A second problem sat underneath the first. ADR-0010 made `append_event`
idempotent on a content key, but only for `TransferCommitted` (keyed on the
transfer id). Account lifecycle events had no key, so `event_dedup_key` returned
`None` and a second append duplicated the row. Any repair path that re-appends
the event therefore needs the event to first gain a stable identity; without it,
recovery cannot tell "the event was already written" from "write it now."

How should a lifecycle transition be made crash-safe, reusing the existing
recovery machinery rather than inventing a parallel one?

## Decision Drivers

* **Same repair path, not a second one**: reuse the `SagaStore` write-ahead
  record + startup `recover()` that the commit engine already has.
* **Idempotent in every crash window**: recovery must converge whether it runs
  before either write, between the two writes, or after both.
* **No duplicate events, no double version bump**: rolling a transition forward
  twice must be a no-op.
* **Remove the triplication**: one transition primitive parameterized by the
  flag mutation, not three near-identical copies.
* **Storage stays dumb** (ADR-0003): the store gains no transition-specific
  method; it still just appends versions and events and follows instructions.

## Considered Options

#### Option 1: Wrap the two writes in one store transaction

Add a store method that appends the version and the event atomically.

**Pros:**

* No write-ahead record; the gap cannot open.

**Cons:**

* Reintroduces a monolithic, guard-bearing store method, the exact thing
  ADR-0003 removed. The store would again bundle domain steps into one
  transaction.
* Does not compose with the existing `recover()`; lifecycle durability would
  diverge from commit durability.

#### Option 2: Write-ahead record + idempotent roll-forward (reuse `recover()`)

Persist a `PendingTransition {next_account, event}` write-ahead record before
either write, keyed in the same `SagaStore` as commit sagas under a tagged
`PendingRecord` enum. `recover()` rolls it forward: append the version only when
it is not yet present (`append_account_version` requires `version == current+1`,
so a version check is the idempotency test), then re-append the event, then
delete the record. To make the event re-append idempotent, give the three
transition events a `version` field and key `event_dedup_key` on
`(account, version)`.

**Pros:**

* One recovery entry point handles commit sagas and account transitions.
* Idempotent in every window: the version check guards the append, and the
  `(account, version)` key collapses a duplicate event.
* Storage stays dumb; no new transition method.
* Lets the three lifecycle methods collapse to one `transition` helper.

**Cons:**

* Changes the event schema (a new field) and generalizes the dedup key type
  from `EnvelopeId` to a string.
* The write-ahead blob format for in-flight commit sagas changes (now wrapped in
  `PendingRecord::Envelope`), so a saga persisted by an older binary would not
  deserialize after upgrade. Acceptable for in-flight, single-lifetime records.

## Decision Outcome

Chosen option: **Option 2**. It closes the gap by reusing ADR-0003's machinery
instead of contradicting it, keeps the store dumb, and removes the triplication
as a side effect.

Concretely:

* A single `Ledger::transition(id, mutate, make_event)` holds the shared shape.
  `freeze`/`unfreeze`/`close` supply only the flag mutation and the event;
  `close` layers its emptiness guard before the call. Because a closed account
  holds no live postings, checking emptiness before the shared not-closed guard
  still surfaces `AccountAlreadyClosed` on a re-close.
* The write-ahead record is a `PendingTransition {next, event}`, stored in the
  `SagaStore` under a tagged `PendingRecord::{Envelope, Transition}` so
  `recover()` dispatches by kind. The transition key is minted from the same
  generator as reservation ids, so it never collides with a commit saga's key.
* `recover()` rolls a transition forward through `complete_transition`: append
  the version if `current.version < next.version`, re-append the (now
  idempotent) event, delete the record. No phase tracking is needed, unlike the
  commit saga, because both steps are individually idempotent.
* `LedgerEventKind::Account{Frozen,Unfrozen,Closed}` gain a `version` field
  (`#[serde(default)]`, so old event JSON still loads). `event_dedup_key`
  returns `Option<String>`: a transfer's lowercase-hex id (unchanged from what
  the SQL `dedup_key` column already holds, so no migration) or an account
  transition's `acct:{id}:{sub}:{version}`. `AccountCreated` keeps no key: it is
  not a version transition and is not re-driven.

### Positive Consequences

* A lifecycle transition interrupted at any point is repaired on the next
  `recover()`, matching the commit path's guarantee.
* Lifecycle events now record which account version they correspond to, so the
  event stream (ADR-0010) can be correlated with the account history.
* Three copies become one primitive.

### Negative Consequences

* The event schema and dedup-key type changed; downstream consumers that
  exhaustively match the event kinds must account for the new field.
* An in-flight commit saga's write-ahead blob written before this change will
  not deserialize afterward. In-flight records have no cross-version durability
  contract, so this is a one-time, accepted cost.
* `create_account`'s create-then-`append_event` has the same shape of gap. It is
  not a version transition and is left for a future change.

## Links

* Refines [ADR-0003](0003-dumb-storage-saga-recovery.md) (extends write-ahead +
  roll-forward recovery to account transitions).
* Refines [ADR-0010](0010-event-stream-vs-transfer-log.md) (generalizes the
  idempotent-event key from transfers to lifecycle transitions).
