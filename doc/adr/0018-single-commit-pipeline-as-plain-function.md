# Single-commit pipeline as a plain function, legend for multi-transfer only

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-07-15
* Targeted modules: `kuatia` (`ledger::commit`, `saga`)
* Associated tickets/PRs: N/A

## Context and Problem Statement

ADR-0002 chose a saga commit pipeline (`reserve -> finalize`) driven by the
`legend` crate, and listed as a positive consequence that "the same steps drive
both a single commit and multi-transfer workflows." In the code this did not
hold literally: multi-transfer workflows compose `PayMovementStep` and
`DepositMovementStep`, which each wrap `commit()`, while `ReservePostingsStep`
and `FinalizeTransferStep` were used only by the single-commit `EnvelopeSaga`.

Driving one fixed two-step sequence through `legend` cost a four-hop call path
(`commit_envelope` -> `drive_envelope_saga` -> `FinalizeTransferStep::execute`
-> `Ledger::finalize_envelope`) plus a `legend!` macro in a separate file. The
finalize step was a trampoline that only forwarded to `finalize_envelope`, and
its compensation was dead code: `legend` compensates completed steps in LIFO
order, so a failing terminal step never runs its own compensation. Should the
single commit keep going through `legend`, or is it clearer as a plain function?

## Decision Drivers

* **Navigability**: reading "how a commit finalizes" should not require
  bouncing across three files and a macro expansion.
* **Honesty of the abstraction**: the durability engine is the phase-tracked
  write-ahead record plus `recover()` (ADR-0003), not `legend`. The saga
  framework contributed only per-step retry and LIFO compensation to the single
  commit, and half of that (the terminal step's compensation) was unreachable.
* **Preserve behavior exactly**: retry counts, the release-on-failure
  compensation, the point-of-no-return semantics, and the recovery model must
  not change.
* **Keep real composition**: multi-transfer workflows must still compose with
  workflow-wide compensation.

## Considered Options

#### Option 1: Keep `legend` driving the single commit (status quo)

**Pros:**

* Good, because it matches ADR-0002's wording literally.
* Good, because retry and compensation are expressed through one framework.

**Cons:**

* Bad, because the single commit is a fixed two-step line, so the saga
  machinery buys little: one trampoline step and one unreachable compensation.
* Bad, because the control flow spans `commit_envelope`, `saga.rs`,
  `envelope_saga.rs`, and the `legend!` macro for one straight-line concept.

#### Option 2: Drive the single commit with a plain function; keep `legend` for multi-transfer

Replace `EnvelopeSaga` with a `drive_envelope_saga` function that runs
`reserve -> finalize` directly: retry each phase, and on a finalize failure
release the reservation (a no-op past the point of no return, where recovery
rolls forward). Delete `ReservePostingsStep`, `FinalizeTransferStep`, and
`envelope_saga.rs`. Keep `PayMovementStep`/`DepositMovementStep` and the
`legend!`-composed multi-transfer sagas unchanged.

**Pros:**

* Good, because the commit reads top to bottom in one place, next to
  `finalize_envelope` and `recover`.
* Good, because it removes the trampoline and the unreachable compensation.
* Good, because it names the truth: `legend` earns its keep across transfers
  (real cross-step reversal), not within one commit.

**Cons:**

* Bad, because the retry-and-compensate logic is now hand-written rather than
  provided by the framework, so it must be kept correct by tests.
* Bad, because it diverges from ADR-0002's "the same steps drive both."

## Decision Outcome

Chosen option: **Option 2**. The single commit becomes a plain function; the
`legend` saga is reserved for multi-transfer composition, which is the only
place its cross-step retry and LIFO compensation are actually exercised. This
refines ADR-0002: the two-phase `reserve -> finalize` structure, the reservation
protocol, and the write-ahead recovery model are all unchanged. Only the driver
of the single commit changes, from a `legend` execution to a function that
replicates the same retry and compensation semantics.

### Positive Consequences

* One reading path for a commit: `commit_envelope` -> `drive_envelope_saga`
  (reserve, then finalize) -> `finalize_envelope`.
* The unreachable terminal-step compensation and the forwarding step are gone.
* The dumb-storage count contract stays centralized in `saga.rs`
  (`verify_postings`, `verify_posting_states`, `ensure`) and is shared by the
  reserve phase and by `finalize_envelope`.

### Negative Consequences

* Per-phase retry and the release-on-failure compensation are now explicit code
  in `drive_envelope_saga`, covered by the recovery tests (which exercise this
  function directly): a `Reserving` saga that fails re-validation releases the
  reservation and aborts; a taken posting makes the reserve phase fail without
  release.
* ADR-0002's "the same steps drive both" no longer applies to the reserve and
  finalize primitives; composition happens at the `commit()` level instead.

## Links

* Refines [ADR-0002](0002-saga-commit-pipeline.md): keeps the two-phase saga and
  its drivers for multi-transfer workflows, replaces the single-commit driver.
* Depends on [ADR-0003](0003-dumb-storage-saga-recovery.md): the write-ahead
  record, phase tracking, and roll-forward recovery the function relies on.
