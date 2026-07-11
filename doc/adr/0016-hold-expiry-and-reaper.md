# Hold expiry via deadline metadata and an in-memory reaper

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-07-10
* Targeted modules: `kuatia` (`ledger`, `inflight`, `expiry`)
* Associated tickets/PRs: N/A

## Context and Problem Statement

Inflight holds ([ADR-0014](0014-inflight-holds-via-holding-accounts.md)) park
funds until a caller confirms or voids them. Nothing bounds that window. An
authorization that is never settled parks its funders' balance forever: the
holding subaccounts stay open, the funds stay out of reach, and only a manual
`void` releases them. This is the authorize/capture pattern with no
authorization timeout, which every real payment rail has.

We want an authorization to carry an optional deadline, and for the ledger to
return expired holds to their funders on its own once that deadline passes, with
no operator action.

Two constraints pull against a naive fix:

* **Derive, don't store.** The rest of the inflight design keeps every fact in
  the authorize transfer's metadata and derives live state from balances. A
  deadline should live there too, not in a new mutable "expiry" table that a
  background job mutates.
* **No polling storm.** A ledger can hold many open authorizations. Waking on a
  timer to scan every open hold on every tick wastes work and still reacts late.
  Expiry should fire close to the deadline without a tight poll loop.

How do we record a per-authorization deadline durably, auto-release on time
without a mutable expiry store, and drive it efficiently rather than by polling?

## Decision Drivers

* **Reuse the commit path.** Auto-release must be the existing `void`, so
  idempotency, conservation, and crash recovery are inherited unchanged. No
  second settlement mechanism.
* **Durable fact, derived state.** The deadline is recorded once in the authorize
  transfer's metadata (the same CBOR payload that already carries the leg table).
  Nothing about expiry is stored mutably.
* **Rebuildable index.** Any in-memory structure that drives timing must be
  reconstructable from the durable metadata on startup, so a crash loses no
  deadline.
* **Fire near the deadline, not on a poll.** The reaper should sleep until the
  earliest known deadline, not wake on a fixed interval.
* **Opt-in and backward compatible.** Existing `authorize` callers and existing
  stored holds (no deadline) keep never expiring.

## Considered Options

#### Option 1: A `SUM`-free periodic scan of open holds

On a fixed interval, list open inflights, read each deadline from metadata, and
void the ones past due.

**Pros:**

* Good, because it needs no new field beyond the deadline and no in-memory index:
  the durable metadata is the only source.
* Good, because it is trivially correct after a crash: the next scan sees the same
  open holds.

**Cons:**

* Bad, because it reacts up to one interval late, and tightening the interval
  turns it into a busy poll over a growing account set.
* Bad, because every tick re-lists and re-decodes all open holds even when nothing
  is due, work proportional to open holds rather than to expiries.

#### Option 2: A persisted expiry queue (new store table / sub-trait)

Add a durable table keyed by deadline, written on authorize and deleted on
settle, that a worker pops from.

**Pros:**

* Good, because pop-until-due is efficient and survives restarts with no rebuild.

**Cons:**

* Bad, because it adds mutable state and a new `Store` sub-trait plus a migration,
  the opposite of ADR-0014's "existing storage only, derive don't store."
* Bad, because it duplicates a fact already implied by the authorize metadata,
  introducing a second source of truth that can drift from the holds it tracks.

#### Option 3: Deadline in metadata, in-memory `BTreeMap` index, reaper task (chosen)

Record an optional `expires_at` (Unix milliseconds) in the existing
`InflightMeta::Authorize` payload. Keep an in-memory
`BTreeMap<deadline, {inflight handles}>` on the `Ledger`, populated on
`authorize_with_expiry` and rebuilt on `recover()` by scanning open inflights and
reading their deadlines back from metadata. A background reaper task sleeps until
the map's earliest key, then voids every handle due at or before now via the
ordinary `void` path and drops them from the map. A `tokio::sync::Notify` wakes
the reaper when a newly authorized hold has an earlier deadline than it is
currently sleeping on.

**Pros:**

* Good, because the deadline is one more field in a payload that is already
  written, hashed, and recovered. No new store, no migration, no mutable table.
* Good, because the index is a pure cache: it is rebuilt from durable metadata on
  startup, so a crash loses no deadline, and a stale entry only causes a harmless
  no-op `void` (a fully settled inflight has zero held balance, so nothing moves).
* Good, because `BTreeMap` gives the earliest deadline in `O(log n)`, so the
  reaper sleeps exactly until the next expiry instead of polling.
* Good, because auto-release is the existing `void`: crash-safe, idempotent,
  conservation-preserving, and self-describing in the audit trail (an expiry is
  indistinguishable in the ledger from a manual void, which is correct — the funds
  went back to the funder either way).

**Cons:**

* Bad, because the index is process-local: the reaper only fires while a ledger
  instance is running with a spawned reaper. A ledger that is down past a deadline
  reaps on next startup (rebuild + immediate due sweep), not at the wall-clock
  instant. Acceptable: the deadline is a floor on when funds *may* return, and the
  hold is still manually voidable meanwhile.
* Bad, because two instances against one store could both run reapers and race to
  void the same hold. Safe by construction — `void` is idempotent and the loser
  settles nothing — but redundant. Running a single reaper per store is the
  intended deployment.

## Decision Outcome

Chosen option: **Option 3**, because it adds expiry without a mutable store or a
second source of truth, reuses `void` and `recover()` wholesale, and drives
timing off the deadline rather than a poll. Concretely:

* **Deadline in the authorize payload.** `InflightMeta::Authorize` gains
  `expires_at: Option<i64>` (Unix ms), decoded with `#[serde(default)]` so holds
  written before this change decode as `None` (never expire).
* **Opt-in API.** `authorize` keeps its signature and records no deadline.
  `authorize_with_expiry(transfer, expires_at)` records the deadline and registers
  the handle in the index. `Authorization` and `InflightStatus` surface the
  deadline; `InflightStatus` reports `InflightState::Expired` when funds are still
  held past the deadline but not yet reaped.
* **In-memory index on the Ledger.** A `Mutex<BTreeMap<i64, BTreeSet<EnvelopeId>>>`
  keyed by deadline, plus a `Notify`. `authorize_with_expiry` inserts and notifies;
  `void` / `confirm_all` deregister (a stale entry is harmless regardless).
* **Rebuild on recover.** `recover()` calls `rebuild_expiry_index()`, which scans
  open inflights, reads each `expires_at` from the authorize metadata, and repopulates
  the map. The durable metadata is the source of truth; the map is a derived cache.
* **Reaper task.** `spawn_expiry_reaper(self: &Arc<Ledger>)` spawns a task that
  loops: read the earliest deadline; if due, void it and remove it; else sleep
  until it, waking early if `Notify` signals a newer, earlier deadline; if the map
  is empty, wait on `Notify`. Dropping the returned handle aborts the task.

### Positive Consequences

* Expiry is a thin layer over `void`, `list_open_inflights`, and the existing
  metadata. Crash recovery and idempotency come for free; no schema change.
* Deadlines survive restarts (rebuilt from metadata) and fire promptly (BTree +
  Notify), without a poll loop or a mutable expiry table.
* An expired hold's release is a normal void in the audit trail, so no new event
  kind or reconciliation is needed.

### Negative Consequences

* The reaper is process-local and at-least-once across instances; a single reaper
  per store is the intended deployment, and redundant reaps are safe no-ops.
* While the ledger is down, deadlines do not fire; they are swept on next startup
  after the rebuild, not at the exact wall-clock instant.

## Links

* Extends [ADR-0014](0014-inflight-holds-via-holding-accounts.md) (inflight holds)
  and reuses [ADR-0003](0003-dumb-storage-saga-recovery.md) (dumb storage, saga
  recovery) unchanged. Builds on [ADR-0012](0012-subaccounts.md) (holds as
  subaccounts).
* Usage and API in [doc/inflight.md](../inflight.md).
