# Conformance-tested storage with an in-memory reference

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-06-29
* Targeted modules: `kuatia-storage` (`store_tests!`, `InMemoryStore`), `kuatia-storage-sql`
* Associated tickets/PRs: N/A

## Context and Problem Statement

`Store` is a trait with several sub-traits and multiple backends (an
in-memory store and a SQL store over SQLite/PostgreSQL), and, since
ADR-0003, the saga's correctness depends on every backend behaving
*identically*, down to the affected-row counts each primitive returns.
How do we guarantee that two independent `Store` implementations have
exactly the same observable semantics, and keep them in lock-step as the
trait evolves?

## Decision Drivers

* **Semantic equivalence**: InMemory and SQL must return the same counts
  and state transitions, or the saga's count interpretation (ADR-0003)
  breaks on one backend.
* **One source of truth for behavior**: the contract should be
  executable, not just prose.
* **Cheap to extend**: adding a backend, or a sub-trait method, should
  make the obligations obvious.
* **Fast feedback**: most semantics should be testable without a
  database.

## Considered Options

#### Option 1: Bespoke tests per backend

Each `Store` impl has its own hand-written test suite.

**Pros:**

* Good, because each suite can exploit backend specifics.

**Cons:**

* Bad, because the two suites drift: a behavior tested for one backend
  may be untested (and divergent) for the other.
* Bad, because there is no single, enforceable definition of "correct
  `Store` behavior."

#### Option 2: Trait documentation only (plus mocks)

Specify semantics in doc comments; let callers mock the store.

**Pros:**

* Good, because it is low-effort up front.

**Cons:**

* Bad, because prose is not executable, so nothing prevents a backend
  from violating it.
* Bad, because mocks encode an *assumed* contract, which can itself be
  wrong.

#### Option 3: A shared conformance suite + an in-memory reference

A `store_tests!` macro generates one suite of `async` tests; every
backend (`InMemoryStore`, `SqlStore`) runs the same suite via its own
factory. `InMemoryStore` doubles as the **executable reference** for the
intended semantics. The convention is that every `Store` sub-trait
method has a conformance test, so new methods force new tests.

**Pros:**

* Good, because both backends are held to the identical, executable
  contract, including the affected-row counts ADR-0003 relies on.
* Good, because `InMemoryStore` is a fast, dependency-free reference for
  the semantics and a ready test double for higher layers.
* Good, because adding a backend is "run the macro with your factory,"
  and adding a sub-trait method is incomplete until its conformance test
  exists.

**Cons:**

* Bad, because the macro suite must stay backend-agnostic (no
  backend-specific assertions), so a few backend-specific behaviors
  still need separate tests.
* Bad, because the in-memory reference must be maintained to match the
  SQL backend exactly, a second implementation to keep honest (which is
  also the point).

## Decision Outcome

Chosen option: **Option 3, a shared `store_tests!` conformance suite
with `InMemoryStore` as the executable reference**, because it is the
only option that *enforces* semantic equivalence across backends (a hard
requirement once the saga interprets counts, ADR-0003), gives a fast
dependency-free reference/double, and makes the obligations for new
backends and new methods explicit.

### Positive Consequences

* Both `InMemoryStore` and the SQL backend pass the same suite; a
  divergence in counts or transitions fails the build.
* New `Store` sub-trait methods come with conformance tests by
  convention.
* Higher layers (ledger, saga) test against the fast in-memory store.

### Negative Consequences

* The conformance suite must remain backend-neutral; genuinely
  backend-specific behavior needs its own tests.
* The in-memory reference is a second implementation that must track the
  SQL one.

## Links

* Underpins [ADR-0003](0003-dumb-storage-saga-recovery.md) (equivalent
  count semantics across backends).
* Background: `crates/kuatia-storage/src/store_tests.rs` and the backend
  test harnesses.
