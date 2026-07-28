# The commit-safety invariant and where each part is enforced

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-07-28
* Targeted modules: `kuatia-core` (`validate`), `kuatia` (`ledger::commit`,
  `saga`)
* Associated tickets/PRs: N/A

## Context and Problem Statement

"What makes a commit safe" had no single place to audit. The invariant was
split across three modules with no anchor tying them together:

* value / conservation / floor / freeze / snapshot / book-policy checks in
  `validate_and_plan` (`kuatia-core`),
* the double-spend / reservation-ownership check inlined inside the
  ~110-line `finalize_envelope` (`kuatia/ledger/commit.rs`),
* the affected-row count contract in `apply_and_verify` / `verify_postings`
  (`kuatia/saga.rs`).

A reader auditing `validate_and_plan` in isolation could reasonably but wrongly
conclude it prevents double-spends; its lifecycle check even carried a comment
saying the real enforcement happens "elsewhere" with no pointer to where. The
load-bearing double-spend guard had no name and was reachable only through an
end-to-end commit, so its unit-test surface was the easy (pure) half only.

Could the whole decision be concentrated into one pure function "given this
envelope and current state, may it commit"? No: the double-spend property is not
decidable against a snapshot. In a concurrent ledger the only way to know a
posting can be spent is to atomically try to spend it and read the result. The
CAS *is* the decision (ADR-0003 dumb storage), and it must not be hoisted next
to `validate_and_plan`, which is pure / sync / no-IO by contract.

## Decision Drivers

* **One audit surface** for the ledger's core safety property, without merging
  the pure value checks and the stateful CAS into a single function.
* **Preserve the pure/async boundary** (ADR-0002, ADR-0003): validation stays
  pure and IO-free; the concurrency authority stays in the saga.
* **Name and unit-test the double-spend guard** rather than leaving it inline
  and only end-to-end testable.

## Decision Outcome

Keep the two halves in their correct layers, but make the map explicit and give
the runtime guard a name. Commit safety is the conjunction of three checks, each
with a single home:

| Invariant | Home | Kind |
|---|---|---|
| Value / conservation / floor / freeze / close / snapshot / book policy | `validate_and_plan` (`kuatia-core::validate`) | Pure, snapshot-in-time, best-effort under concurrency |
| Double-spend / reservation ownership | `consume_reserved` (`kuatia::saga`), called by `finalize_envelope` | Runtime CAS, authoritative under contention |
| Affected-row count contract after each dumb write | `apply_and_verify` / `verify_postings` (`kuatia::saga`) | Interpretation of storage counts |

Concretely:

* The double-spend guard was extracted out of `finalize_envelope` into
  `consume_reserved`, co-located with the count-contract helpers it depends on.
  It consumes only the rows this saga reserved
  (`deactivate_postings(_, Some(rid))`) and then asserts every consumed id is
  `Spent`; that assertion can pass only when no id was left active or held by
  another saga. It has direct unit tests (spends our reservation; refuses an
  unreserved posting; refuses one held by another saga).
* `validate_and_plan`'s lifecycle comment now points at `consume_reserved` and
  this ADR instead of a vague "elsewhere".

### Positive Consequences

* One documented map of the commit-safety invariant; each part is a named,
  independently testable seam.
* The authoritative double-spend guard is unit-testable without an end-to-end
  commit.

### Negative Consequences

* The invariant is still physically split across `kuatia-core` and `kuatia`.
  That split is intentional (pure value checks vs. runtime CAS) and this ADR is
  the anchor that makes it navigable; it is not a single code seam.

## Links

* Builds on [ADR-0003](0003-dumb-storage-saga-recovery.md) (dumb storage, the
  best-effort floor/freeze note) and [ADR-0002](0002-saga-commit-pipeline.md).
* Relates to [ADR-0006](0006-reservation-protocol-posting-lifecycle.md)
  (reservation protocol / posting lifecycle).
