# Reversal via compensating transfers, not deletion

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-06-29
* Targeted modules: `kuatia` (`ledger::reverse`)
* Associated tickets/PRs: N/A

## Context and Problem Statement

Sometimes a committed transfer must be undone: a mistaken payment, a saga
compensating a later failure. The ledger's whole value proposition is being
auditable by replaying an append-only log (ADR-0001). How do we undo a
transfer without breaking that property?

## Decision Drivers

* **Audit integrity**: history must never be rewritten; "what happened"
  includes the mistake and its correction.
* **Consistency with the model**: an undo should use the same machinery as a
  normal commit, not a special back door.
* **Idempotency**: undoing twice (e.g. a retried compensation) must not
  double-undo.
* **Composability**: saga compensation needs a reliable, reusable undo.

## Considered Options

#### Option 1: Delete or mutate the original postings/record

Physically remove the created postings (and restore consumed ones), or edit the
transfer record to "undo" it.

**Pros:**

* Good, because the post-undo state is "clean", as if it never happened.

**Cons:**

* Bad, because it destroys history: the ledger is no longer reconstructible by
  replay, defeating ADR-0001.
* Bad, because it is a privileged mutation path outside the normal commit
  model, with its own concurrency and crash hazards.

#### Option 2: A `void`/`reversed` flag on the transfer

Mark the original as voided rather than deleting it.

**Pros:**

* Good, because the original row is preserved.

**Cons:**

* Bad, because balances now depend on interpreting flags, not just summing
  postings: the projection is no longer "sum of `Active` postings."
* Bad, because it adds mutable state to an otherwise append-only record and a
  second code path for "is this transfer effective?"

#### Option 3: A compensating transfer (an inverse envelope)

`reverse(id)` loads the original transfer and builds an inverse envelope: it
consumes the original's created postings and recreates the original's consumed
ones as new postings, then commits it through the normal `commit_envelope`
path. Nothing is deleted or mutated; the reversal is itself a content-addressed
transfer.

**Pros:**

* Good, because history is fully preserved: both the original and its reversal
  appear in the log; balances remain "sum of `Active` postings."
* Good, because it reuses the exact commit path (reserve → validate →
  finalize, recovery, idempotency) with no special undo machinery.
* Good, because it is idempotent: the reversal envelope is deterministic and
  content-addressed, so committing it twice returns the same receipt.
* Good, because it gives saga compensation a reliable, uniform primitive.

**Cons:**

* Bad, because the post-undo state is not "as if it never happened": there are
  now two transfers (intended), which a naive reader might find noisy.
* Bad, because reversing is a real commit and is subject to the same validation
  (e.g. it cannot reverse postings already consumed by a later transfer without
  that surfacing as a normal failure).

## Decision Outcome

Chosen option: **Option 3: reversal as a compensating transfer**, because it
is the only option that preserves the append-only, replayable audit log
(ADR-0001) while reusing the normal, already-hardened commit and recovery
path, and is idempotent by content-addressing.

### Positive Consequences

* `reverse()` is a thin wrapper over `commit_envelope`; saga finalize
  compensation uses it to undo a committed step.
* The audit trail shows the original and the correction; balances stay a pure
  projection over `Active` postings.

### Negative Consequences

* Undo is visible as a second transfer (by design), not an erasure.
* A reversal is subject to normal validation/availability: it can legitimately
  fail if the world moved on, surfacing as an ordinary error.

## Links

* Preserves the audit model of
  [ADR-0001](0001-modified-utxo-signed-postings.md); reuses
  [ADR-0002](0002-saga-commit-pipeline.md) /
  [ADR-0003](0003-dumb-storage-saga-recovery.md).
* Background: [transfers.md](../transfers.md) ("Reversal").
