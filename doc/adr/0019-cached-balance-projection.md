# Cached balance projection maintained by a projection service

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-07-17
* Targeted modules: `kuatia` (`ledger/balance.rs`, `ledger/projection.rs`),
  `kuatia-storage` (`Store`), `kuatia-storage-sql` (a new `balance_projection`
  table). The chosen outcome leaves the commit path untouched and adds no lease
  table or projector service (see Decision Outcome).
* Associated tickets/PRs: N/A

## Context and Problem Statement

Balance is never stored; `compute_balance` (`crates/kuatia/src/ledger/balance.rs`)
fetches every live posting for a `(account, subaccount, asset)` and sums it in
Rust on every read. Under the signed-posting UTXO model (ADR-0001) each `pay`
mints a change posting and nothing coalesces or caps the active set, so a read
is `O(N live postings)` and `N` grows unbounded for hot accounts. The commit-time
overdraft check pays the same cost.

We want fast reads without weakening correctness, without doing arithmetic in the
store, and without a central authority handing out sequence numbers.

## Decision Drivers

* **Read cost**: remove the unbounded `O(N live postings)` sum from the read path.
* **Correctness independent of the projector**: reads must be correct even if the
  projection service is stopped, lagging, crashed, or running many instances.
* **No math in the store**: the store keeps a scalar and serves rows; all summing
  happens in Rust, in the service (honours the arithmetic-in-Rust rule).
* **No central sequence authority**: keep decentralized, time-ordered snowflake
  ids (AutoId); do not add a global counter every commit must serialize on.
* **Authority stays with the postings**: the append-only postings remain the
  source of truth (ADR-0017); the cache is a disposable, rebuildable projection,
  never authoritative for the validate-time overdraft check.

## Key insight (why no sequence is needed)

A balance fold is commutative: `snapshot + Σcreates − Σconsumes`. Applying
committed transfers in any order yields the same total, so the cursor needs only
**completeness** (never miss a committed transfer) and **exactly-once** (never
double-apply), never a total order.

Correctness is carried entirely by the read rule:

```
balance(account, asset) = projection.snapshot
                        + Σ over committed transfers with commit_time > projection.watermark
                              of creates(+) / consumes(−) for (account, asset)
```

Both `creates` and `consumes` live on each committed `EnvelopeRecord`, and
`transfer_accounts` indexes every transfer under its involved set (created ∪
consumed owners), so a single per-account stream contains both the create and the
spend. This read is correct whatever the projector is doing; the projector only
shortens the tail.

## Considered Options

#### Option 1: Status quo (sum live postings on every read)

* Bad, because reads and the validate path are `O(N live postings)`, unbounded
  under fragmentation.

#### Option 2: Central strictly-increasing commit sequence

* Bad, because it needs a coordination point every commit serializes on, and has
  an assignment-order-vs-visibility hazard (a slow `seq=5` visible after `seq=6`
  is skipped by a reader past 6). Rejected: it reintroduces the central authority
  we are avoiding.

#### Option 3: Projection service + per-account locking + commit-time watermark

An out-of-store service maintains a `balance_projection` cache. Reads are
lock-free (`snapshot + tail`). The service rebuilds an account's snapshot under a
per-account lease; the watermark is a commit-time cutoff with a grace window, so
nothing new ever appears below it. No sequence.

* Good, because correctness is independent of the service (reads always work).
* Good, because there is no central authority: ordering is the existing
  decentralized AutoId time order, and coordination is per-account, not global.
* Good, because it fits ADR-0017 (a disposable, rebuildable hot index) and keeps
  arithmetic out of the store.
* Bad, because it is the first aggregate cache, adds a lease/coordination
  primitive, and rests on a bounded commit-to-visibility lag assumption (with the
  reconciliation rebuild as the backstop).

## Decision Outcome

Chosen option: **append-only cache points with a lazy, volume-based trigger** (a
refinement of Option 3 that keeps its watermark-with-grace and lock-free reads
but drops the service, lease, and CAS in favor of append-only rows and a
read-triggered append). Concretely:

1. **Append-only `balance_projection` table.** One row per cache point:
   `(id, account, subaccount, asset, balance, watermark)`, where `id` is a
   Rust-minted monotonic snowflake and `balance` is a `Cent` stored as TEXT.
   Rows are only ever inserted, never updated; a cache point is history. This is
   the append-only value-table pattern (ADR-0017).

2. **Reads select the closest cache point at or before the target time.**
   `get_closest_balance_projection(account, asset, as_of)` returns the row with
   the largest `watermark ≤ as_of` (tie-broken by highest `id`), so a read never
   uses a snapshot that covers transfers committed after `as_of`. The ledger
   passes the current time by default; a past `as_of` yields an as-of balance.

3. **Commit-time watermark with grace.** A cache point's watermark is
   `now − Δ`, measured in commit/visibility time (`EnvelopeRecord.created_at`,
   stamped at `store_transfer`), never intent time, so nothing new appears below
   it. `Δ` is not business latency: an inflight hold that settles hours later
   commits (and is stamped) at settlement time, landing in the tail. Default: 60
   seconds, configurable.

4. **Lazy, volume-based trigger.** No commit hook, no background loop. When a
   read folds the tail and finds at least `snapshot_interval` credits/debits
   (created + consumed postings owned by the account) accrued since the closest
   cache point, it appends a new one off the read path (a background task). The
   threshold is configurable (default 128).

5. **Append-only makes coordination free.** Concurrent appends just add rows; a
   read takes the closest-at-or-before, so there is no lock, CAS, or lease. A
   redundant or slightly-stale append is harmless history that a later read
   ignores. Reads return `closest snapshot + Rust-folded tail` and are correct
   regardless of whether any cache point exists; `compute_balance` (the
   authoritative full live-posting sum) is retained and remains what the validate
   path reads.

6. **Authority unchanged; UTXO stays the concurrency control.** The
   validate-time overdraft check keeps reading the authoritative live-posting
   (UTXO) sum, never the cache. The projection is a read accelerator only; a
   stale snapshot must never admit an overdraft. This matters most for an account
   with `DEBIT_MUST_NOT_EXCEED_CREDIT`: the signed-posting UTXO model plus the
   reservation protocol (ADR-0006/0016) is what makes the no-overdraft invariant
   exact under concurrency. A commit atomically reserves the specific postings it
   spends, so two concurrent debits cannot both consume the same value, and the
   balance floor is enforced against the real live set at commit time. An
   eventually-consistent cache cannot provide that guarantee, so the UTXO path is
   retained internally as the concurrency-control mechanism for these accounts,
   with the projection layered on top purely to speed up reporting reads.

7. **Reconciliation.** Appending a cache point (or reading with a very small
   interval) folds from the append-only postings and detects no drift by
   construction, since a read always equals the live sum. `compute_balance` is
   the from-scratch recompute available for an explicit audit.

### Positive Consequences

* Balance reads drop from `O(N live postings)` toward `O(tail since the latest
  cache point)`, bounded by how recently a read last appended, not by lifetime
  fragmentation.
* Correctness lives in "closest snapshot + full tail" over the append-only log;
  the cache points are pure optimization. Best-effort and crash-safe by
  construction: a lost or absent cache point only lengthens a tail.
* No central sequence, no lease, no background service; append-only rows plus
  closest-at-or-before selection need no coordination.

### Negative Consequences

* First aggregate cache: more surface than the row-copy hot indexes.
* Append-only cache points accumulate; pruning old ones (by count or age) is
  future work.
* A write-heavy account that is never read never appends a cache point, so its
  first read folds a long tail (then appends). Acceptable: the cost tracks reads.
* Rests on a bounded commit-to-visibility lag assumption (the grace `Δ`); the
  watermark must be commit-time, not intent time, or a late settlement below the
  watermark would be lost.
* The projection must never become authoritative for the overdraft check.

## Links

* Builds on [ADR-0017](0017-correctness-first-append-only-hot-indexes.md)
  (disposable, rebuildable hot indexes) and
  [ADR-0016](0016-immutable-postings-index-tables.md).
* Balance model: [ADR-0001](0001-modified-utxo-signed-postings.md).
* Dumb-storage primitives and recovery:
  [ADR-0003](0003-dumb-storage-saga-recovery.md).
* Inflight settlement timing (why the watermark is commit-time):
  [ADR-0014](0014-inflight-holds-via-holding-accounts.md).
* Background: [accounts.md](../accounts.md), balance read path in
  `crates/kuatia/src/ledger/balance.rs`.
