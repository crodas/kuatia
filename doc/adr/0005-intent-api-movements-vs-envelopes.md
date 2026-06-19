# Intent API: movements and transfers over raw envelopes

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-06-29
* Targeted modules: `kuatia-types` (`Movement`, `Transfer`, `TransferBuilder`), `kuatia` (`ledger::resolve`)
* Associated tickets/PRs: N/A

## Context and Problem Statement

In a UTXO-style model (ADR-0001) a commit ultimately operates on concrete
postings: which exact postings to consume, which to create, and the change.
But making callers assemble that (pick inputs, compute change, balance per
asset) is error-prone and couples application code to the ledger's internals.
What should the *public* unit of intent be, and where does the translation to
concrete postings happen?

## Decision Drivers

* **Ergonomics and safety**: callers should express *what* they want ("pay B
  40 USD from A"), not hand-select UTXOs.
* **Keep the UTXO model an implementation detail**: selection and
  change-making should not leak into application code.
* **Determinism and idempotency**: the same intent must resolve
  deterministically, and re-submitting must be a no-op.
* **Composability**: multi-account, multi-asset events (FX, compound entries)
  must be expressible as one atomic intent.

## Considered Options

#### Option 1: Callers build `Envelope`s directly (raw UTXO API)

The public API is the resolved form: callers choose consumed posting ids and
construct created postings.

**Pros:**

* Good, because it is maximally explicit and gives full control (useful for FX
  or hand-tuned flows).

**Cons:**

* Bad, because every caller re-implements posting selection, change-making, and
  per-asset balancing, which is easy to get wrong.
* Bad, because it couples application code to posting ids and the UTXO model.
* Bad, because there is no natural high-level vocabulary
  (pay/deposit/withdraw).

#### Option 2: A two-layer API: intent (`Movement`/`Transfer`) to resolved (`Envelope`)

Callers express intent as `Movement { from, to, asset, amount }` values grouped
into a `Transfer` (via `TransferBuilder::pay/deposit/withdraw/movement`). The
ledger's `resolve()` turns intent into a concrete `Envelope` by selecting
postings and computing change; the envelope is what gets validated and
committed. The raw envelope path remains available internally (e.g. for
`reverse()` and hand-built multi-asset envelopes).

**Pros:**

* Good, because the common cases are one call and the UTXO mechanics stay
  hidden.
* Good, because intent is small and serializable, and `resolve` is
  deterministic, so the resolved `Envelope` has a stable content id
  (idempotency, ADR re: content-addressing).
* Good, because compound and multi-asset events are just multiple movements
  committed atomically; deposits and withdrawals are movements against a
  boundary account.
* Good, because the escape hatch (build an `Envelope` directly) still exists
  for flows the intent vocabulary cannot express.

**Cons:**

* Bad, because there are two representations to understand
  (intent vs. resolved) and the word "posting" is a noun here, not the
  accounting verb.
* Bad, because idempotency keys on the *resolved* envelope id, so resolution
  must be deterministic for re-submits to dedupe, a property the resolver must
  hold.

## Decision Outcome

Chosen option: **Option 2, the two-layer intent API**, because it keeps the
UTXO model an internal detail, makes the common operations trivial and safe,
and yields a deterministic resolved `Envelope` whose content id gives
idempotency, while still allowing a pre-built envelope for advanced flows.

### Positive Consequences

* `TransferBuilder` offers `pay`/`deposit`/`withdraw` (preferred) over raw
  `movement` construction; one `Transfer` can carry many movements committed
  atomically.
* `commit(transfer)` = `resolve` (read-only) then `commit_envelope`; the saga
  and recovery operate on the resolved envelope (see ADR-0002/0003).
* Deposits resolve to two movements that cancel to zero net debit on the system
  account, so no posting selection is needed.

### Negative Consequences

* Two representations to document; the noun/verb "posting" caveat
  (see [accounting-mapping.md](../accounting-mapping.md)).
* Resolution must stay deterministic so re-submitting the same intent dedupes.

## Links

* Builds on [ADR-0001](0001-modified-utxo-signed-postings.md); committed by
  [ADR-0002](0002-saga-commit-pipeline.md).
* Background: [transfers.md](../transfers.md), [accounting-mapping.md](../accounting-mapping.md).
