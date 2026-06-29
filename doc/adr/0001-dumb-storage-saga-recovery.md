# ADR 0001 — Dumb storage + durable saga recovery

Status: Accepted

## Context

A prior change (commit `c715bd3`, "Make the Store the atomic invariant boundary
for commits") introduced `CommitStore::commit_transfer` — a single database
transaction that bundled ~8 responsibilities: deactivate consumed postings,
insert created postings, persist the transfer record, index `transfer_accounts`
(both created and consumed owners), check `CappedOverdraft` CAS balance guards,
check account-version guards, enforce reservation ownership, and append events.
It also left two confusing public commit entry points (`Ledger::commit` for
intent, `Ledger::commit_atomic` for a pre-resolved envelope) that both funneled
into it.

Two problems motivated revisiting this:

1. **The storage layer carried a lot of domain assumptions.** A "dumb record
   keeper" was the stated goal, yet `commit_transfer` interpreted state, decided
   idempotency, enforced guards, and chose error semantics.
2. **Crash recovery was never finished.** `SagaStore`
   (`save_saga`/`list_pending_sagas`/`delete_saga`) and `legend`'s pause/resume
   plumbing existed but nothing used them; `ExecutionResult::Paused` was treated
   as an error. So the saga ran entirely in memory and `commit_transfer`'s single
   transaction was the *only* crash-safety.

## Decision

Invert the design.

- **Storage is a dumb instruction follower.** Every `Store` write method applies
  one update and returns the **number of affected rows** (`Result<u64,
  StoreError>`), or a genuine I/O error. It never interprets the count, decides
  state, enforces idempotency, or compensates. The conditional `WHERE` clause is
  the instruction; the count is the result. `commit_transfer`, `CommitStore`,
  `CommitRequest`, and the semantic write-outcome `StoreError` variants
  (`Conflict`, `ReservationMismatch`, `PostingNotActive`, `PostingInactive`) are
  removed. The write primitives are `reserve_postings`, `release_postings`,
  `deactivate_postings`, `insert_postings`, `store_transfer(record, involved)`,
  and an idempotent `append_event` (dedup on the transfer id).

- **The saga owns interpretation and idempotency.** A commit is the saga calling
  those primitives in sequence and reading each count: full = continue; partial =
  error → compensate; **zero = read state and continue only if this same
  envelope/reservation already applied it**. (`verify_postings` in `saga.rs`.)

- **One commit path.** `commit(transfer)` resolves the intent into an envelope
  (read-only) then runs `commit_envelope`, the envelope saga (reserve → validate
  → finalize). `commit_envelope(envelope)` serves pre-built/FX envelopes;
  `reverse()` uses it. `commit_atomic` is gone.

- **Durable recovery via write-ahead + roll-forward.** `commit_envelope`
  persists a `PendingSaga {envelope, reservation}` via `SagaStore` before
  mutating anything, and deletes it on terminal. `Ledger::recover()` (startup)
  force-completes any surviving pending saga through the idempotent primitives,
  using the original reservation. It does **not** re-run reserve/validate (those
  reject already-consumed postings); it converges from a crash at any point
  (pre-reserve / reserved / mid-finalize). Because recovery is roll-forward, the
  reservation protocol never leaves orphaned `PendingInactive` postings, so no
  separate reconciliation pass is needed.

`legend`'s pause/resume is for external waits, not crash checkpoints, so durable
recovery is this write-ahead layer around legend, not serialization of the
in-flight execution.

## Consequences

- **Double-spend safety: preserved.** It comes from the reservation protocol —
  `reserve_postings` is a single atomic conditional update, so two sagas cannot
  both claim the same posting.
- **Crash-safety: preserved, differently.** Not one transaction, but write-ahead
  + idempotent roll-forward. Nothing is silently lost; a crash mid-finalize is
  completed by `recover()`.
- **Overdraft floor + freeze/close guards: now best-effort under concurrency.**
  They are checked at validation time, not re-checked atomically at commit (the
  `cas_guards`/`account_guards` and their commit-time re-check are removed). A
  concurrent, unrelated balance change or a freeze/close between validation and
  finalize has a small TOCTOU window. Accepted tradeoff for a dumb storage layer.
- **Simpler, more testable surface.** Storage has no domain logic; all commit
  correctness lives in one place (the saga) with per-primitive count-conformance
  tests and crash-injection recovery tests.

This supersedes the `c715bd3` atomic-boundary decision and parts of the
`93e35fe` follow-up (the conditional-update/guard hardening of `commit_transfer`).
