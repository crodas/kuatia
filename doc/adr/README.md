# Architecture Decision Records

Significant, hard-to-reverse design decisions for Kuatia, captured so the
*why* survives. New ADRs follow [`template.md`](template.md) (MADR-style:
context → drivers → considered options with pros/cons → decision outcome
→ consequences → links). Numbering is sequential; an ADR is never edited
to reverse a decision. Instead, a new ADR supersedes it.

## Index

| ADR | Title | Status | Summary |
|-----|-------|--------|---------|
| [0001](0001-modified-utxo-signed-postings.md) | Modified UTXO: value as signed postings | accepted | Value is signed postings (negative = "offset positions"), not mutable balances; conservation is structural; balances are projections. |
| [0002](0002-saga-commit-pipeline.md) | Saga commit pipeline | accepted | Commit is a compensating saga (`reserve → finalize`), not a single/distributed transaction: composable, coordinator-free, crash-recoverable. |
| [0003](0003-dumb-storage-saga-recovery.md) | Dumb storage + durable saga recovery | accepted | Storage returns affected-row counts and makes no decisions; the saga owns interpretation/idempotency; crash-safety is phase-tracked write-ahead + roll-forward. Refines 0002. |
| [0004](0004-account-policies-overdraft-model.md) | Account policies & overdraft model | superseded by 0018 | A closed `AccountPolicy` enum per account gated negative postings + floor. Refined 0001. Superseded by 0018, which collapses the enum into a single flag. |
| [0005](0005-intent-api-movements-vs-envelopes.md) | Intent API: movements vs. envelopes | accepted | Callers express `Movement`/`Transfer` intent; `resolve()` produces the concrete `Envelope`. UTXO mechanics stay internal; idempotency keys on the resolved id. |
| [0006](0006-reservation-protocol-posting-lifecycle.md) | Reservation protocol & posting lifecycle | accepted (storage representation superseded by 0016) | `Active → PendingInactive → Inactive` + a durable `ReservationId` give lock-free, recoverable, exclusive ownership of inputs. The primitive behind 0002/0003. |
| [0007](0007-reversal-via-compensating-transfers.md) | Reversal via compensating transfers | accepted | Undo is an inverse envelope committed through the normal path (never deletion/mutation), preserving the append-only audit log. |
| [0008](0008-conformance-tested-storage.md) | Conformance-tested storage | accepted | One `store_tests!` suite every backend must pass, with `InMemoryStore` as the executable reference; enforces the equal count semantics 0003 relies on. |
| [0009](0009-monetary-representation-integer-minor-units.md) | Monetary amounts as integer minor units | accepted | `Cent` is an `i64` newtype of minor units with only checked arithmetic; scale lives in the presentation-only `Amount`, not on the stored value or asset. Makes 0001's conservation exact. |
| [0010](0010-event-stream-vs-transfer-log.md) | Derived event stream vs. transfer log | accepted | A secondary append-only `EventStore` feed (outbox-style) for transfer + account-lifecycle events; transfer log stays authoritative. `append_event` is idempotent on a content key, a scoped exception to 0003. |
| [0011](0011-swappable-money-backing.md) | Swappable integer backing for money, default i64 | accepted | `Cent` moves to a `kuatia-money` crate over a `CentBacking` trait; the i64↔i128 width is a cargo feature, hidden from the API, stored as text. Refines 0009. |
| [0012](0012-subaccounts.md) | Subaccount dimension on account identity | accepted | Account identity gains a subaccount dimension (used for per-destination inflight holding subaccounts). |
| [0013](0013-journaling-model.md) | Journaling model: transfer as journal entry | accepted | A committed `Transfer`/`Envelope` is a (compound) journal entry; the transfer log is the accounting journal; `Book` is a policy scope, not the journal. Frames 0001/0005/0010 in accounting terms. |
| [0014](0014-inflight-holds-via-holding-accounts.md) | Inflight holds via per-destination holding accounts | accepted | A hold is a subaccount of its destination; committing routes funds through the holding subaccount so pending value stays visible and reconcilable until settle or cancel. |
| [0015](0015-fixed-width-account-code.md) | Fixed-width 20-character account code | accepted | The IBAN-style code becomes a fixed 20 chars (18-char body + 2 trailing check digits, five groups of four) by packing id (63 bits) and subaccount (30 bits) into one permuted value. Presentation-only; caps the subaccount at `SUB_BITS`. Supersedes the code section of 0012. |
| [0016](0016-immutable-postings-index-tables.md) | Immutable postings with active/reserved index tables | accepted (hot-table representation refined by 0017) | Postings become an insert-only immutable table; lifecycle state moves to two index tables (`active_postings`, `reserved_postings`). Append-only integrity + least privilege (no `UPDATE`) + hot working set by segregation. Supersedes the storage representation of 0006. |
| [0017](0017-correctness-first-append-only-hot-indexes.md) | Correctness-first storage: append-only value tables, disposable hot indexes | accepted | The guiding principle: value/audit tables (`postings`, `accounts`) are strictly append-only source of truth; disposable hot tables (`active_postings`, `reserved_postings`, `account_head`) index the live subset by `INSERT`/`DELETE` only and are rebuildable. Correctness first; no `UPDATE` anywhere, enforceable by DB grants. Generalizes 0016 (hot tables now hold full row copies). |
| [0018](0018-single-debit-must-not-exceed-credit-flag.md) | Collapse account policy into a single debit-must-not-exceed-credit flag | accepted | The `AccountPolicy` enum is removed; the only per-account balance constraint is the `AccountFlags` bit `DEBIT_MUST_NOT_EXCEED_CREDIT`. Overdraft is allowed by default (unbounded); the flag forbids a negative balance and negative postings. Credit-line floors become an application concern. Supersedes 0004. |

## Recommended future ADRs

Real decisions whose rationale lives in the code/docs but is not yet
captured as an ADR, roughly in priority order:

1. **Content-addressed transfer ids, and rejecting a sequential hash
   chain**: `EnvelopeId = double-SHA-256(canonical bytes)` for idempotency
   + tamper evidence, and why a per-transfer hash chain was rejected (a
   concurrency bottleneck). See the "No Sequential Hash Chain" section of
   `architecture.md`.
2. **Pure core / async layer split**: a zero-IO, deterministic
   `kuatia-core` (validation, selection, hashing; golden-vector testable)
   vs. the async storage + saga layer.
3. **Rust-generated ids (`AutoId`), no `AUTOINCREMENT`/`SERIAL`**: the
   application owns identity (snowflake-style `i64`), enabling future
   sharding without DB coordination.
4. **Append-only, versioned accounts + snapshot pinning**: accounts are
   never modified in place; snapshot hashes guard against TOCTOU between
   load and apply.
5. **All arithmetic in Rust, never in SQL**: no `SUM`/`MAX`/etc. on
   monetary values; the storage layer stays a dumb record keeper.
6. **Posting proliferation & consolidation**: greedy largest-first
   selection (ADR-0001/0005) fragments balances into ever-smaller change
   postings; whether and how to consolidate, and what to do with dust, is
   undecided.
7. **Retention / pruning of spent postings and the append-only logs**: the
   immutable postings table (ADR-0016), the transfer log, and the derived
   event stream (ADR-0010) all grow without bound; archival/retention is
   deferred and currently a conscious omission. ADR-0016 makes the spent
   history physically separable from the live working set, which is where
   pruning would apply.
8. **Read/projection consistency model**: a balance is a non-transactional
   sum over the live (active or reserved) postings, so a read concurrent
   with a commit is eventually consistent; the read-side guarantee is
   implied but never stated.
