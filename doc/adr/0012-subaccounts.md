# Extend account identity with a subaccount dimension

* Status: accepted
* Authors: Cesar Rodas
* Date: 2026-07-05
* Targeted modules: `kuatia-types`, `kuatia-core`, `kuatia-storage`,
  `kuatia-storage-sql`, `kuatia` (`ledger`), `kuatia-dashboard`
* Associated tickets/PRs: N/A

## Context and Problem Statement

An account was identified by a single `i64` (`AccountId`). Some workloads need to
partition one account's holdings into several distinct balances under the same
owner: sub-ledgers, per-purpose buckets, earmarks, or reservations that are
individually addressable and drained or closed independently, without minting
unrelated top-level accounts. Classical accounting calls the general shape a
control account with a subsidiary ledger; payment and banking systems call it a
sub-ledger or a set of virtual accounts under a master account.

We want that structure as a first-class part of account identity: an account is a
base id plus a **subaccount**, and each partition is a full account record with
its own policy, so no special-case code is needed to give a partition its own
overdraft rule or lifecycle. The default subaccount (`0`) is the account's main
account, so existing behaviour is unchanged when subaccounts are not used.

## Decision Drivers

* **Partitioning and attribution**: several balances under one owner, each
  addressable and discoverable as a subaccount of the base account.
* **Per-partition policy**: a subaccount must be able to carry its own policy,
  flags, book, and version, independent of the base account.
* **Segregated balances**: a base account's subaccounts must never be silently
  summed into a single figure.
* **Query by account or by subaccount**: reads must span all subaccounts or
  restrict to one.
* **Least churn and preserved invariants**: the change touches every layer that
  keys on an account; conservation, double-spend, and floor checks must be
  unchanged.

## Considered Options

#### Option 1: Fold the subaccount into `AccountId` (a composite `{id, sub}`, chosen)

Make the account identity itself two legs: `AccountId { id: i64, sub: i64 }`, with
`sub = 0` the main account. Aggregate reads take a base `id: i64` plus an optional
subaccount filter.

**Pros:**

* Good, because there is one identity type: posting owners, movement endpoints,
  account records, and balance keys are all `AccountId`, so per-subaccount
  balances fall out of the existing keys with no new wrapper type.
* Good, because "query by account or by subaccount" is explicit: base reads take
  `(id: i64, sub: Option<i64>)` — `None` spans every subaccount, `Some(s)`
  restricts to one — while entity ops take the full `&AccountId`.
* Good, because each `(id, sub)` is a full account record with its own policy.

**Cons:**

* Bad, because callers that want a base handle read `account.id` rather than
  passing a distinct base type; the split between base reads (`i64`) and exact
  entity ops (`&AccountId`) has to be kept clear.
* Bad, because it is a large, cross-crate change (the identity gains a field, `.0`
  accesses become `.id`) plus a schema migration.

#### Option 2: A separate `AccountRef { account, sub }` owner/identity type

Keep `AccountId` as the i64 base and add a separate `AccountRef` wrapper as the
owner/endpoint/entity identity.

**Pros:**

* Good, because the base `AccountId` stays a bare i64, so aggregate "all
  subaccounts" reads keep a natural base handle.

**Cons:**

* Bad, because it adds a second account-identity type (`AccountId` vs
  `AccountRef`) that every layer has to convert between.
* Bad, because it is the same cross-crate churn as Option 1 without collapsing to
  a single identity.

#### Option 3: Subaccounts as balance buckets that inherit the parent policy

Track a subaccount only on postings, with the account entity keyed by base id and
its policy shared by all subaccounts.

**Pros:**

* Good, because the `accounts` table does not change.

**Cons:**

* Bad, because a subaccount cannot carry its own policy. A partition that must
  stay `NoOverdraft` under a `SystemAccount`/overdraft base account could not,
  so any structural guarantee that depends on the partition's own policy is lost.

## Decision Outcome

Chosen option: **Option 1, fold the subaccount into `AccountId`**, because a
single two-leg identity keeps balances naturally segregated, lets every
subaccount carry its own policy, and avoids carrying two account-identity types.

### The identity type

`AccountId { id: i64, sub: i64 }` (in `kuatia-types`). `sub = 0` is the main
account; a non-zero `sub` is a subaccount. `sub` is an `i64`, the same type as
the base id, so it stores directly in a `BIGINT` column with no cast.
Constructors and helpers:

* `AccountId::new(id)` — the main account `{ id, sub: 0 }`.
* `AccountId::with_sub(id, sub)` — a specific subaccount.
* `base()` — the main account of an id (`sub` set to `0`).
* `is_main()` — whether `sub == 0`.

`AccountId` derives `Copy`/`Eq`/`Hash`/`Ord` and its canonical `ToBytes` is the
base id followed by the subaccount (both big-endian), so the subaccount is folded
into every content hash (envelope ids, posting ids, account snapshots).

### IBAN-style account code

`AccountId` has an IBAN-style string form, so an identifier carries a checksum
and a mistyped one is rejected before it reaches the store. The machine format
is two ISO 7064 mod-97 check digits followed by a 26-character base-36 body,
with no country code. `to_grouped()` adds a space every four characters for
display.

The body does not encode the raw legs directly. The `(id, sub)` pair is first
run through a keyed 128-bit Feistel permutation, then each 64-bit half is
base-36 encoded (13 characters each). Without this, small sequential ids would
render as near-zero codes that leak their value and order, and a base account
and its subaccount would share a visible prefix. After the permutation the codes
look random and unrelated. Under the default seed, `AccountId { id: 5, sub: 7 }`
renders `221RDWNSN4VCQNK2NN42KJFSAOLI` (grouped `221R DWNS N4VC QNK2 NN42 KJFS
AOLI`).

`FromStr` ignores spaces and dashes, upper-cases the input, checks the
structure, and **validates the mod-97 checksum** (returning `ParseAccountIdError`
on failure), then inverts the permutation to recover the two legs. Each half is
read as a `u64` bit pattern and reinterpreted as `i64`, so any value
round-trips.

The permutation key is a process-global seed with a built-in default, settable
once at startup with `set_id_seed` (the dashboard exposes it as `--id-seed` /
`KUATIA_ID_SEED`). Changing the seed changes every code, so it must be stable
across a deployment. This is obfuscation, not security: anyone with the seed can
decode a code, so it is not a substitute for authorization.

This is a presentation and edge form only. Storage and low-level usages keep the
two `i64` legs: the SQL schema, the `Store` trait signatures and query types,
in-memory keys, `ToBytes`, and serde (`{id, sub}`) are unchanged, so there is no
migration and no content-hash impact. The dashboard exposes the string as a
`code` field and routes account pages by the machine form (`/accounts/<code>`),
parsing and checksum-validating it at the route boundary. `Debug` keeps the short
`id` / `id.sub` form for logs.

### The entity model

Each `(id, sub)` is its own **full account record** with its own `policy`,
`flags`, `book`, `version`, `user_data`, and `metadata`. The main account is
`(id, 0)`. A subaccount is created, versioned, frozen, and closed exactly like any
other account (closing still requires zero live postings), and its policy is
enforced independently — a subaccount can be `NoOverdraft` while its base account
is not, or vice versa.

`AccountId` is the owner of a posting (`Posting.owner`, `NewPosting.owner`,
`NewPosting.payer`), the endpoint of a movement (`Movement.from`/`to`), the id of
an `Account`, and the subject of an `AccountSnapshotId`. `TransferBuilder::pay` and
`movement` move between main accounts; `pay_ref` and `movement_ref` move between
specific subaccounts.

### Reads: by account or by subaccount

Entity operations take the full `&AccountId` (exact): `get_account`,
`get_accounts`, `append_account_version`, `get_account_history`. Aggregate reads
take a base `id: i64` plus an optional subaccount:

* `get_postings_by_account(id: i64, sub: Option<i64>, asset, status)` and
  `get_transfers_for_account(id: i64, sub: Option<i64>)` span every subaccount
  when `sub` is `None` and one when `Some(s)`.
* `PostingQuery`/`TransferQuery` carry a base `account: i64` and `sub:
  Option<i64>`.

### Balances are always segregated

Balances are reported per subaccount and never summed across them:

* `Ledger::balance(&AccountId, &AssetId) -> Cent` reads exactly one subaccount.
* `Ledger::balances(&AccountId, &AssetId, sub: Option<i64>) -> Vec<SubAccountBalance>`
  returns one entry per non-closed subaccount (`sub = None` spans all, `Some(s)`
  filters to one). There is deliberately no API that sums across subaccounts.
* `Ledger::list_subaccounts(&AccountId) -> Vec<AccountId>` lists the non-closed
  subaccounts of a base account.

Closed subaccounts are excluded from the aggregate reads. This inverts the
classical control-account expectation (where a parent's balance is the sum of its
subsidiaries) on purpose: a base account does **not** roll up its subaccounts.

### Validation and books

Per-asset conservation and the balance-floor / negative-posting checks operate on
the full `AccountId` owner, so they are per subaccount and use each subaccount's
own policy. Book membership is scoped by **base account**: a book that lists a
base account (or matches its flags) admits all of that account's subaccounts.

### Storage schema and migration

* `accounts` primary key becomes `(id, subaccount, version)`. `postings` gain a
  `subaccount` column and `idx_postings_owner` widens to `(owner, subaccount,
  asset, status)`. `transfer_accounts` gains `subaccount` in its key and index.
* The subaccount is an `i64`, so it stores directly in a `BIGINT` column with no
  cast (an opaque id, compared only for equality in SQL, never as a magnitude).
* A `002_subaccounts` migration adds the column (existing rows default to
  `subaccount = 0`, the main account) and rebuilds `accounts` /
  `transfer_accounts` for the widened primary keys, since SQLite cannot alter a
  primary key. `001_init.sql` is left intact. The in-memory store keys accounts by
  the composite `AccountId` directly.

### Positive Consequences

* One account can carry several independent balances, each a full account record
  with its own policy, discoverable via `list_subaccounts` and attributable by
  shared base id.
* Balances are always presented per subaccount, so a main account and its
  subaccounts are never accidentally summed into one figure.
* Conservation, double-spend, and floor guarantees are unchanged; they simply key
  on the full `(id, sub)` owner.

### Negative Consequences

* Every content hash changes (the subaccount is folded into `AccountId`'s
  canonical bytes) and the schema migrates. Existing data upgrades in place to
  `subaccount = 0`.
* Because accounts are append-only and never deleted, each subaccount that is
  created and later closed leaves a permanent record (its versions plus its
  inactive postings); the accounts and postings tables grow with the number of
  subaccounts ever created, not the number currently open.
* `list_subaccounts` and any "open subaccounts" scan currently read all account
  rows and filter in memory, so they pay for closed subaccounts; a store with many
  historical subaccounts would want an index on the not-closed set.
* The base-id-vs-full-`AccountId` split (aggregate reads take `i64`, entity ops
  take `&AccountId`) has to be kept clear at call sites.

## Links

* Builds on [ADR-0001](0001-modified-utxo-signed-postings.md) (signed postings)
  and [ADR-0003](0003-dumb-storage-saga-recovery.md) (dumb storage).
* Usage: [doc/accounts.md](../accounts.md), [doc/glossary.md](../glossary.md).
