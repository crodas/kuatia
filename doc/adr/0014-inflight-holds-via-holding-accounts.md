# Inflight holds via per-destination holding accounts

> Revised by [ADR-0012](0012-subaccounts.md): a hold is now a
> **subaccount** of its destination rather than a standalone account, and the
> "one open inflight per account" rule is dropped (a destination hosts many
> concurrent inflights, one per distinct trade). The rest of this ADR still
> holds.

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-07-03
* Targeted modules: `kuatia` (`ledger`, `saga`), `kuatia-types`
* Associated tickets/PRs: N/A

## Context and Problem Statement

Callers need to reserve funds for a trade without settling it yet: authorize the
whole trade now, then later confirm it (in full or in parts) or void it. This is
the authorization/capture pattern, applied to a multi-leg trade rather than a
single payment. A confirmed trade like

```
A -> B   -> 100 EUR
B -> A   -> 0.1 BTC
A -> fee -> 0.0001 BTC
B -> fee -> 1 EUR
```

should be expressible in an inflight form where the funds leave the payers now
but are parked until each leg is confirmed or returned.

The tension is with the ledger's core model. Kuatia is append-only and
UTXO-style: value is signed postings that move between accounts, balances are
derived (never stored), and there is no mutable state to update in place. The
only reservation concept today is the transient `PendingInactive` posting status
stamped with a `ReservationId`. That is a short-lived concurrency primitive owned
by a single in-flight saga, not a durable, user-facing hold. Recovery
(`Ledger::recover`) treats any `PendingInactive` posting as a saga to complete or
release, and a hold can stay open far longer than a saga.

A further constraint: this must use the existing storage. No new `Store`
sub-trait and no migration. The accounts table and the transactions table have to
carry everything.

How do we represent a durable, partially-confirmable, multi-leg reservation
without adding mutable state, a parallel recovery mechanism, a new store, or
domain logic in the storage layer?

## Decision Drivers

* **Append-only**: a hold must be a durable fact recorded in the ledger, not a
  lock held in memory or a transient status.
* **Reuse the commit path**: authorize, confirm, and void should ride the
  existing content-addressed, idempotent, crash-safe `commit_envelope` saga and
  its `recover()` roll-forward, not a second bespoke mechanism.
* **No mutable balance / derive, don't store**: the amount still held must be a
  derived balance, not a counter decremented on each confirmation.
* **Existing storage only**: state lives in the accounts and transactions tables.
  No new store, no migration, no arithmetic pushed into SQL.
* **Self-describing via metadata**: the inflight facts (the transaction id, the
  leg table, the role of each transfer, the funder per leg) are carried in the
  metadata of the holding accounts and the transfers, so the lifecycle is read
  from recorded fields rather than inferred from movement direction.
* **Safety by construction**: no double-spend, and no confirming more than was
  authorized, enforced by existing invariants rather than new guards.
* **Auditability and simplicity**: the full history of a request should be
  readable from the ledger, and the feature should add as little surface as
  possible.

## Considered Options

#### Option 1: Promote the transient `PendingInactive` reservation to a durable hold

Keep the payers' postings in place and hold them as `PendingInactive` for the
whole authorization window. Available balance already excludes `PendingInactive`,
so the funds would look reserved.

**Pros:**

* Good, because it reuses the existing reservation stamp and moves no funds.
* Good, because the available-vs-ledger balance split already models "reserved
  but not spent."

**Cons:**

* Bad, because it breaks recovery: `recover()` treats every `PendingInactive`
  posting as an in-flight saga to roll forward or release, so a durable hold would
  be torn down or double-driven by the first startup recovery pass.
* Bad, because a reservation is all-or-nothing on a whole posting. Partial
  confirmation has no representation, and there is nowhere to keep the change from
  a partial confirmation.
* Bad, because it conflates a short-lived concurrency primitive with a long-lived
  business state, and pins postings under a saga-owned lock for an unbounded time.

#### Option 2: Add a new posting state (`Held`) for reserved funds

Introduce a fourth posting status that keeps funds attached to the payer but
marks them reserved for a specific request, with a new primitive to split a held
posting on partial confirmation.

**Pros:**

* Good, because the reservation is explicit and the funds visibly stay on the
  payer.

**Cons:**

* Bad, because a new state touches every layer: the balance rule, validation, the
  store trait and both backends, recovery, and the whole conformance suite.
* Bad, because partial confirmation forces a posting-splitting primitive, which is
  new domain logic pushed back toward the store.
* Bad, because it grows a special case into a model whose whole point is that
  value is just postings moving between accounts. Larger surface, higher risk.

#### Option 3: Rewrite each destination to a per-destination holding account (chosen)

Model an inflight transaction as the ordinary trade with every destination
`to` rewritten to a fresh holding account created for that destination:

```
A -> B.inflight   -> 100 EUR
B -> A.inflight   -> 0.1 BTC
A -> fee.inflight -> 0.0001 BTC
B -> fee.inflight -> 1 EUR
```

Committing that rewritten transfer is the authorize step: one atomic,
conservation-preserving commit moves the funds out of A and B into the holding
accounts. That commit is stored in the transactions table like any other, and its
content-addressed `EnvelopeId` is the inflight handle. The metadata carries the
record across every artifact: the authorize transfer's metadata declares its role
and the full leg table `[(destination, hold, funder, asset, amount)]`; each
holding account's metadata records its role and its destination; each later
confirm or void transfer's metadata records its role, the inflight handle, and the
leg it settles. The metadata is therefore the record of what is held and for whom,
and it is content-addressed into each transfer's id, so it is tamper-evident. A
hold is keyed by destination,
so `fee.inflight` legitimately holds two assets funded by two different accounts.

The lifecycle operations are ordinary commits, each driven from the leg table in
the authorize transfer's metadata:

* **Confirm all (no amount)**: for each leg, sweep the holding account's balance
  to its destination. The net effect equals the original confirmed trade.
* **Partial confirm**: commit `X.inflight -> X` for a slice. The remainder stays
  held.
* **Void**: for each leg, return the holding account's remaining balance to the
  funder named in the leg table.

A hold closes when its balance reaches zero; the transaction is terminal when all
its holds are closed. Whether a leg was confirmed or voided is read from the
transactions table (the leg's settling transfer goes to the destination on
confirm, to the funder on void).

Each hold is a **subaccount** of its destination, keyed by a value derived from
the submitted trade. Because the key is trade-specific, a destination hosts
**many concurrent inflights at once**, one per distinct trade, each isolated in
its own subaccount. Re-authorizing the identical trade collides with its own
existing holds and is rejected, while different trades to the same destination
run side by side.

**Pros:**

* Good, because it adds no posting state, no saga phase, no new store, and no
  migration. Every operation is an existing `commit`, so idempotency, content
  addressing, and crash recovery are inherited unchanged.
* Good, because the amount still held is the holding account's balance, a derived
  value. Nothing mutable is stored or decremented.
* Good, because partial confirmation is just another transfer, with change handled
  by the normal resolve step.
* Good, because over-confirmation is impossible by construction: the holding
  account is `NoOverdraft`, so a confirmation exceeding its balance fails
  validation. The sum of confirmations can never exceed the authorized amount.
* Good, because concurrent confirmations serialize on the shared holding posting
  via the reservation protocol, so double-spend safety and the over-confirm bound
  hold under contention with no new locking.
* Good, because void routing reads the funder from the stored authorize transfer,
  so it needs no change to `resolve()` and no reliance on posting provenance.
* Good, because the request's entire history is the holds and the transactions
  that touch them: the authorize, each confirmation, and the void.

**Cons:**

* Bad, because it creates one holding account per destination per request.
  Mitigated by closing terminal holds so they leave the working set. Accounts are
  cheap, snowflake-keyed rows.
* Bad, because a single `(hold, asset)` co-funded by two payers cannot cleanly
  split a partially-confirmed remainder back to each funder on void; out of scope,
  documented. Each `(destination, asset)` is expected to have a single funder, as
  in the example.
* Bad, because voiding returns funds to the original payer, so that account must
  still be open to receive them.

## Decision Outcome

Chosen option: **Option 3, per-destination holding accounts, backed by existing
storage**, because it is the only option that expresses a durable,
partially-confirmable, multi-leg hold purely as existing ledger primitives, with
no new store. It adds no mutable state, reuses the commit and recovery path
wholesale, and gets double-spend and over-confirm safety from invariants the
ledger already enforces. Concretely:

* **Authorize rewrites destinations.** For an inflight transaction, each movement
  `from -> to` becomes `from -> hold(to)`, where `hold(to)` is a fresh
  `NoOverdraft` account flagged `INFLIGHT` whose metadata records its destination.
  The rewritten transfer is committed normally with the leg table in its metadata,
  and its content-addressed `EnvelopeId` becomes the inflight handle.
* **Metadata is the record.** Confirm and void load the authorize transfer with
  `get_transfer` / `get_transfers_for_account` and read the leg table and funders
  straight from its metadata, rather than reconstructing them from movement
  directions. No side record and no new store. Because each hold is a subaccount
  of its destination (sharing its base id), `get_transfers_for_account` on the
  destination's base id surfaces its open holds without a side index.
* **Confirm and void are ordinary transfers, tagged in metadata.** Confirm-all
  commits `hold -> destination` for each leg's balance; partial confirm commits
  `hold -> destination` for a slice; void commits `hold -> funder` per leg, with
  the funder taken from the leg table. Each settling transfer carries its role
  (`confirm` or `void`), the inflight handle, and the leg it settles in metadata.
  All go through `commit`, so all are idempotent and crash-safe. Confirm accepts a
  batch of legs to settle in one call, expressed with the same
  `(from, to, asset, amount)` shape as `TransferBuilder::pay` (`from` the funder,
  `to` the destination); the movements settle in order, each its own commit, so a
  batch is not atomic.
* **State is derived.** The amount held on a leg is `balance(hold, asset)`. The
  authorized amount is the leg's amount in the metadata leg table. Confirmed is
  authorized minus held. Whether a leg was confirmed or voided is read from the
  role tag on its settling transfer, not inferred from where the funds went.
* **Termination closes the holds.** When a hold balance reaches zero, close it
  (legal, since it then has zero active postings). The inflight transaction is done
  when all its holds are closed.

Because void reads the funder from the leg table in metadata rather than from
posting provenance, `resolve()` is left unchanged (the change posting keeps its
current `payer: None`).

### Inflight metadata schema

The payload is a single CBOR-encoded tagged enum stored under one `inflight` key
in the existing `Metadata` map (`BTreeMap<String, Vec<u8>>`), via `ciborium`.
This supersedes the earlier per-key big-endian byte layout: one typed value
instead of hand-packed fields.

```rust
enum InflightMeta {
    Authorize { legs: Vec<InflightLeg> }, // on the authorize transfer
    Hold { destination: AccountId },      // on each holding account
    Confirm { tx: EnvelopeId, destination: AccountId }, // on a confirm transfer
    Void { tx: EnvelopeId, destination: AccountId },    // on a void transfer
}
```

Each `InflightLeg` is `{ destination, hold, funder, asset, amount }`. The
inflight handle is the authorize transfer's content-addressed `EnvelopeId`; the
`tx` field on a settling transfer back-references it.

Open holds are discovered by scanning `INFLIGHT`-flagged, not-closed accounts and
reading their `Hold` metadata; the flag is the marker (metadata is carried, not
queried). Everything semantic (leg table, funders, per-transfer role) is read
from the enum. Because metadata is hashed as opaque bytes into each transfer id,
the payload is tamper-evident; `ciborium` is deterministic for a fixed value.

### Positive Consequences

* The feature is a thin layer over `commit`, `create_account`, `get_transfer`,
  `balance`, and `close`. Crash recovery, idempotency, and conservation come for
  free, and no storage schema changes.
* The over-confirm bound and double-spend safety are structural: they follow from
  `NoOverdraft` and the reservation protocol, with no request-specific checks.
* The audit trail is self-describing: a request's holds and the transactions that
  touch them fully reconstruct what was authorized, confirmed, and returned.

### Negative Consequences

* One holding subaccount per destination per request. Terminal holds are closed
  to bound the open working set, but the accounts table still grows with history
  (as postings and transfers already do).
* A single `(hold, asset)` co-funded by two payers has an ambiguous
  partially-confirmed remainder on void; out of scope, documented.
* Voiding depends on the payer account remaining open. A policy for holds that
  outlive their payer is out of scope here.

## Links

* Builds on [ADR-0001](0001-modified-utxo-signed-postings.md) (signed postings)
  and [ADR-0003](0003-dumb-storage-saga-recovery.md) (dumb storage, saga
  recovery). Reuses the `commit_envelope` path and `recover()` unchanged, and adds
  no new store.
* Background: [architecture.md](../architecture.md) (commit pipeline, posting
  lifecycle, resolve and change outputs), [accounts.md](../accounts.md) (policies,
  account lifecycle).
* Usage and API to be documented in `doc/inflight.md`.
