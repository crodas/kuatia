# Dumb storage + durable saga recovery

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-06-29
* Targeted modules: `kuatia-storage`, `kuatia-storage-sql`,
  `kuatia` (`ledger`, `saga`), `kuatia-core`
* Associated tickets/PRs: N/A

## Context and Problem Statement

ADR-0002 chose a saga commit pipeline. Its finalize step still funneled
everything into one monolithic store transaction,
`CommitStore::commit_transfer`, which bundled ~8 responsibilities (deactivate,
insert, store record, index both sides, CAS balance guards, account-version
guards, reservation authorization, event append) into a single database
transaction. Two problems surfaced: (1) the storage layer carried a lot of
domain assumptions (it interpreted state, enforced guards, decided idempotency
and error semantics), undercutting the "dumb record keeper" goal; and (2)
crash recovery was designed (`SagaStore`, `legend` pause/resume) but never
wired, so that single transaction was the *only* thing protecting against a
half-applied commit. What is the right division of responsibility between the
store and the saga, and how is a commit made crash-safe without that monolithic
transaction?

## Decision Drivers

* **Dumb storage**: the store should follow instructions and report results,
  not make domain decisions; correctness logic should live in one testable
  place.
* **Crash-safety without a global transaction**: recovery must converge from a
  crash at any point and never commit something that did not validate or
  consume postings it does not own.
* **No silent divergence / no double-spend**: preserve the unconditional
  double-spend guarantee from the reservation protocol.
* **Testability**: per-primitive conformance tests and crash-injection recovery
  tests over a small, well-defined surface.

## Considered Options

#### Option 1: Store is the atomic invariant boundary (`commit_transfer`)

Keep one store method that atomically applies the whole commit and enforces all
guards inside a single transaction.

**Pros:**

* Good, because atomicity and crash-safety on a single database are trivial.
* Good, because all guards (floor, version, reservation) are re-checked
  atomically with the write.

**Cons:**

* Bad, because the store is no longer dumb: it interprets state, decides
  idempotency, enforces domain guards, and chooses error semantics.
* Bad, because it pins crash-safety to one transactional store and does not
  compose with the saga's multi-step / multi-resource ambitions (ADR-0002).
* Bad, because the commit/abort logic is split between the saga and a large
  store method, making correctness hard to locate and test.

#### Option 2: Dumb storage + saga interpretation + durable recovery

Storage write methods apply one update and return the **number of affected
rows** (or an I/O error); they never interpret the count, decide state, enforce
idempotency, or compensate. The saga interprets counts (full = continue;
partial = error → compensate; zero = read state and continue only if this same
envelope/reservation already applied it) and verifies end-states. Crash-safety
is a **phase-tracked write-ahead record** (`PendingSaga {envelope, reservation,
phase}`) plus **idempotent roll-forward** in `Ledger::recover()`.

**Pros:**

* Good, because the store is a thin record keeper with no domain logic; all
  commit correctness lives in the saga and is unit-testable.
* Good, because it composes with the saga model and does not require a single
  all-encompassing transaction.
* Good, because recovery is correct by construction: a `Reserving` saga is
  re-run and **re-validated**; a `Finalizing` saga (already validated, owns its
  postings) is rolled forward through a verified path; nothing commits unless
  all consumed postings are confirmed `Inactive`.

**Cons:**

* Bad, because correctness now depends on the saga implementing idempotency and
  end-state verification precisely (no DB safety net).
* Bad, because guards that were re-checked atomically inside `commit_transfer`
  (CappedOverdraft floor, freeze/close) become **best-effort**: re-validated as
  the last step before the writes, but not strictly atomic with them.

## Decision Outcome

Chosen option: **Option 2, dumb storage + saga interpretation + durable
recovery**, because it is the only option that keeps the storage layer dumb and
composable with the saga while still providing crash-safety and unconditional
double-spend safety. Concretely:

* **Storage primitives** return `Result<u64, StoreError>`: `reserve_postings`,
  `release_postings`, `deactivate_postings`, `insert_postings`,
  `store_transfer(record, involved)`, and an idempotent `append_event`. The
  monolithic `commit_transfer` / `CommitStore` / `CommitRequest` and the
  semantic write-outcome error variants are removed.
* **The saga owns the commit**: two steps, `reserve → finalize`. Validation
  runs inside finalize, as its last action before writing, the tightest-window
  floor and freeze/close re-check. `finalize_envelope` verifies every end-state
  and never creates/stores unless **all** consumed postings are `Inactive`.
* **One commit path**: `commit(transfer)` resolves then runs `commit_envelope`;
  `reverse()` runs the same path; there is no separate raw/atomic entry point.
* **Durable recovery**: a `PendingSaga` is written before any mutation
  (`Reserving`), bumped to `Finalizing` at the point of no return. `recover()`
  branches on the phase; the record is deleted only on commit or a clean
  pre-finalize abort, and roll-forward (not rollback) means no orphaned
  `PendingInactive` postings to reconcile.

`legend`'s pause/resume is for external waits, not crash checkpoints, so
durable recovery is this write-ahead layer around `legend`, not serialization
of the in-flight execution.

### Positive Consequences

* The storage surface is small, uniform (counts, not verdicts), and covered by
  a shared conformance suite that both the in-memory and SQL backends pass.
* All commit/abort/recovery correctness is in the saga, exercised by
  crash-injection tests (re-drive `Reserving`, roll forward a partial finalize,
  abort+release when an account is frozen, refuse to double-spend a taken
  posting).
* Double-spend safety is unconditional (reservation protocol).

### Negative Consequences

* **CappedOverdraft floor and freeze/close are tightest best-effort, not
  strictly atomic.** Finalize re-validates immediately before the writes (and
  on the recovery path), shrinking the check-to-write window to one step, but
  without folding the check into the write (a CAS) or per-account
  serialization, a concurrent commit in that last sub-step gap can still breach
  a floor. If hard floors are ever required, a follow-up ADR should add
  per-`(account, asset)` serialization or a narrow commit-time CAS.
* The saga must keep its idempotency and verification invariants exact; the DB
  no longer provides a rollback safety net.

## Links

* Refines [ADR-0002](0002-saga-commit-pipeline.md) and supersedes its
  monolithic `commit_transfer` finalize.
* Builds on [ADR-0001](0001-modified-utxo-signed-postings.md).
* Background: [architecture.md](../architecture.md) (commit pipeline, recovery,
  the floor-under-concurrency section), [accounts.md](../accounts.md).
