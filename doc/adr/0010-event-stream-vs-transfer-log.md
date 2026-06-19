# A derived event stream alongside the transfer log

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-06-29
* Targeted modules: `kuatia-storage` (`EventStore`, `LedgerEvent`), `kuatia` (`saga`, `ledger`)
* Associated tickets/PRs: N/A

## Context and Problem Statement

The transfer log is the append-only source of truth for value (ADR-0001):
balances are projected by summing `Active` postings, and nothing else is
authoritative. But applications need to *react* to the ledger (update a read
model, notify a downstream service, drive an outbox), and not everything worth
reacting to is a value transfer: accounts are also created, frozen, unfrozen
and closed. Polling the transfer log misses the lifecycle events and forces
every consumer to re-derive "what changed." Do we need a second log, what is
authoritative, and what are its delivery/idempotency semantics?

## Decision Drivers

* **Observability without re-deriving**: consumers want a single ordered feed
  of "what happened," not a diff of the transfer table.
* **Non-transfer events exist**: account create/freeze/unfreeze/close are
  meaningful occurrences that are not movements of value.
* **One source of truth**: value authority must stay with the transfer log
  (ADR-0001); a notification feed must not become a second, competing
  authority.
* **Plays with saga recovery**: a committed transfer can be re-driven by
  recovery (ADR-0003), so the event emitted for it must not duplicate on
  replay.
* **Fits dumb storage, mostly**: append should be a simple primitive; any
  deviation from "return a count" (ADR-0003) must be deliberate and justified.

## Considered Options

#### Option 1: No event log; consumers tail the transfer log

Derive all reactions from the append-only transfer records.

**Pros:**

* Good, because there is only one log to store and reason about.

**Cons:**

* Bad, because account lifecycle changes (freeze/close) are not transfers and
  have nowhere to appear, so a whole class of events is invisible.
* Bad, because every consumer must re-implement "tail and interpret
  transfers," coupling them to transfer internals and offering no uniform
  subscription point.

#### Option 2: Event log as the source of truth (full event sourcing)

Make an event stream authoritative and fold balances from events.

**Pros:**

* Good, because there is a single authoritative narrative and reactions are
  first class.

**Cons:**

* Bad, because it conflicts head-on with ADR-0001: balance would become a fold
  over events rather than a sum of `Active` postings, and the UTXO/posting
  model would be demoted to a projection of the event log.
* Bad, because it creates two ways to be "true" (postings vs. events) that
  must be kept consistent, exactly the divergence ADR-0001 set out to avoid.

#### Option 3: A secondary, derived append-only event stream (outbox-style)

Keep the transfer log authoritative. Add an `EventStore`: `LedgerEvent { seq,
timestamp, kind }` where `kind` is `TransferCommitted { transfer_id }` or an
account-lifecycle event. The store assigns a monotonic `seq` and exposes
`append_event` + `get_events_since(after_seq, limit)`. `append_event` is
**store-side idempotent** on `event_dedup_key`: replayable events
(`TransferCommitted`, re-driven by saga recovery) dedup on the transfer id and
return the existing `seq`; events with no natural identity (account lifecycle)
return `None` and may recur. The feed is *derived*: it observes what the
authoritative writes already decided.

**Pros:**

* Good, because value authority stays with the transfer log (ADR-0001); the
  event stream is an observation feed, not a second source of truth.
* Good, because lifecycle events that are not transfers finally have a home,
  and consumers get one ordered, tailable feed (`get_events_since`) instead of
  re-deriving from transfers.
* Good, because dedup on the transfer id makes `TransferCommitted` survive saga
  recovery's re-drive without emitting a duplicate: at-least-once upstream
  becomes effectively-once for the events that carry a content identity.
* Good, because it is an outbox the saga appends to as its last step,
  decoupling downstream delivery from the commit's critical path.

**Cons:**

* Bad, because it is a second append-only log to persist, index by `seq`, and
  retain.
* Bad, because `append_event` deviates from the dumb-storage "return an
  affected-row count" rule (ADR-0003): the store assigns `seq` and performs the
  dedup, since both are storage-native and the key is content-based, not a
  state-machine decision. A deliberate, narrow exception.
* Bad, because lifecycle events have no dedup key and **may duplicate** on
  retry/recovery, so consumers must tolerate at-least-once for those.
* Bad, because `seq` orders events but is not a causal/transactional clock; it
  is an emission order, not a serialization of value state.

## Decision Outcome

Chosen option: **Option 3, a derived, append-only event stream alongside the
authoritative transfer log**, because it gives applications a uniform, ordered
feed (including non-transfer lifecycle events) without challenging ADR-0001's
"transfer log is the only authority on value." Making `append_event` idempotent
on a content-based `event_dedup_key` is what lets the saga emit it safely under
recovery re-drive; accepting that keyless lifecycle events may recur keeps the
model honest about at-least-once delivery. The store-side `seq` assignment and
dedup are a consciously scoped exception to dumb storage (ADR-0003), justified
because both are intrinsic storage concerns rather than domain decisions.

### Positive Consequences

* One subscription point (`get_events_since`) for read models, outboxes and
  notifications; consumers no longer reverse-engineer the transfer table.
* `TransferCommitted` is effectively-once thanks to transfer-id dedup, aligning
  with the saga's idempotent re-drive (ADR-0003) and content-addressed
  transfers.
* Balances remain a pure projection of `Active` postings; the event stream adds
  no competing authority.

### Negative Consequences

* A second append-only log to store and retain (no pruning policy is defined
  yet, a candidate future ADR alongside posting/log retention).
* `append_event` is a documented exception to the count-returning storage
  contract.
* Account-lifecycle events may be delivered more than once; consumers must be
  idempotent. `seq` is emission order, not a causal clock.

## Links

* Subordinate to [ADR-0001](0001-modified-utxo-signed-postings.md) (transfer
  log is the source of truth; events are derived).
* Idempotent emission under recovery:
  [ADR-0003](0003-dumb-storage-saga-recovery.md); the `append_event` exception
  is scoped against that ADR's dumb-storage contract.
* Dedup key shares the content-identity logic behind reversal idempotency
  ([ADR-0007](0007-reversal-via-compensating-transfers.md)).
* Background: `crates/kuatia-storage/src/events.rs` (`LedgerEvent`,
  `event_dedup_key`, `EventStore`).
