# Accounts

## Overview

An account is a versioned entity that owns postings. Balance is never
stored: it is always computed from postings for a given (account, asset)
pair. `balance()` sums the live postings, meaning those in the active or
reserved index (`Active ∪ Reserved`); spent postings, which remain only in
the immutable table, are excluded.

## Structure

| Field | Type | Description |
|-------|------|-------------|
| `id` | `AccountId { id: i64, sub: i64 }` | Stable identity: a base id plus a subaccount (`sub = 0` is the main account) |
| `version` | `u64` | Starts at 1, increments on every mutation |
| `policy` | `AccountPolicy` | Balance floor rule (see below) |
| `flags` | `AccountFlags` | Lifecycle flags (`FROZEN`, `CLOSED`) + user-defined (`USER_0` to `USER_7`) |
| `book` | `BookId` | Book this account belongs to |
| `metadata` | `Metadata` | `BTreeMap<String, Vec<u8>>` for free-form data |

## Subaccounts

An `AccountId` is a base `id` plus a `sub`. `sub = 0` is the account's main
account; a non-zero `sub` is a subaccount of the same base id. Each `(id, sub)`
is a full account record with its own policy, flags, book, version, and
lifecycle, created, versioned, frozen, and closed exactly like any other
account. A subaccount can be `NoOverdraft` while its base account is not, or the
reverse, because every check keys on the full `AccountId`.

Subaccounts partition one owner's holdings into several individually addressable
balances (sub-ledgers, earmarks, reservations) without minting unrelated
top-level accounts. Helpers on `AccountId`: `new(id)` (main account),
`with_sub(id, sub)`, `base()` (the main account of an id), and `is_main()`.

`AccountId` also has an IBAN-style string form (`Display` / `FromStr`): a fixed
20 characters, an 18-character base-36 body then two trailing ISO 7064 mod-97
check digits, with no country code (e.g. `KUJL QEL8 IX2X GTBK 4425`). The body
packs the base id (63 bits) and the subaccount (`SUB_BITS` bits) into one 93-bit
value and runs it through a keyed format-preserving permutation before encoding
(and inverts it on parse), so a code does not reveal the raw ids; the key is a
global seed with a default, configurable via `set_id_seed`. Parsing validates the
checksum, so a mistyped identifier is rejected. This is obfuscation, not security
(the seed decodes it), and a presentation/routing form only; storage keeps the
two `i64` legs. The fixed width is what caps the subaccount at `SUB_BITS`; see
ADR-0015.

Balances are always reported per subaccount and are never summed across them:

- `balance(&AccountId, &AssetId)` reads exactly one subaccount.
- `balances(&AccountId, &AssetId, sub)` returns one entry per non-closed
  subaccount (`sub = None` spans all, `Some(s)` filters to one).
- `list_subaccounts(&AccountId)` lists the non-closed subaccounts of a base id.

A base account does **not** roll up its subaccounts: there is deliberately no
API that sums across them. Aggregate reads take a base `id: i64` plus an
optional subaccount filter (`get_postings_by_account`,
`get_transfers_for_account`); exact entity operations take the full
`&AccountId`. Book membership is scoped by base account: a book that lists a base
account admits all of that account's subaccounts. See
[adr/0012-subaccounts.md](adr/0012-subaccounts.md).

## Policies

Each account has a policy that controls what balance constraints apply:

| Policy | Balance floor | Negative postings | CAS guard |
|--------|--------------|-------------------|-----------|
| `NoOverdraft` | `>= 0` | No | No |
| `CappedOverdraft { floor }` | `>= floor` | Yes (down to floor) | Yes |
| `UncappedOverdraft` | None | Yes (unbounded) | No |
| `SystemAccount` | None | Yes | No |
| `ExternalAccount` | None | Yes | No |

An overdraft is represented as a negative posting (an offset position)
assigned to the account to cover a shortfall. When an account's positive
postings are insufficient for a debit, the resolve step consumes them all
and creates a negative posting for the remainder. `NoOverdraft` accounts
forbid this; validation rejects any transfer that would create a negative
posting on a `NoOverdraft` account. `CappedOverdraft`'s floor bounds how
negative the balance may go; `UncappedOverdraft`, `SystemAccount`, and
`ExternalAccount` are unbounded.

`CappedOverdraft`'s floor is re-validated as the last step before finalize
writes (the finalize step re-loads balances and account versions and
re-runs validation just before deactivating). This is the tightest
best-effort: the check-to-write window is one step, not the whole saga. It
is not strictly atomic. A concurrent commit in that last gap can still
breach the floor (write-skew). Double-spend safety is unaffected. The
reservation protocol (an atomic conditional `reserve_postings`) guarantees
a posting cannot be consumed twice. See
[accounting-mapping.md](accounting-mapping.md) and the ADR at
[adr/0003-dumb-storage-saga-recovery.md](adr/0003-dumb-storage-saga-recovery.md).

## Lifecycle

Accounts follow a three-state lifecycle controlled by flags:

```
Created (v1) → Frozen (v2) → Unfrozen (v3) → Closed (v4)
                  ↑               │
                  └───────────────┘
```

| Operation | Precondition | Effect |
|-----------|-------------|--------|
| `freeze(id)` | Not closed | Sets `FROZEN` flag, increments version |
| `unfreeze(id)` | Frozen | Clears `FROZEN` flag, increments version |
| `close(id)` | Zero active postings | Sets `CLOSED` flag, increments version |

- **Frozen** accounts reject all transfers (both debits and credits).
- **Closed** accounts reject all transfers and cannot be reopened.
- Closing requires zero active postings for all assets.

## Append-Only Versioning

Accounts are never modified in place. Each mutation appends a new version:

```
Version 1: { policy: NoOverdraft, flags: ∅ }         ← created
Version 2: { policy: NoOverdraft, flags: FROZEN }     ← frozen
Version 3: { policy: NoOverdraft, flags: ∅ }         ← unfrozen
```

The store enforces `version_new == version_current + 1`, preventing gaps or
overwrites. The full history is queryable via `account_history(id)`.

## Snapshot Pinning

Transfers can carry `AccountSnapshotId` values: pairs of `(AccountId,
snapshot_hash)` recording which account version the transfer was validated
against.

During validation, if snapshots are present, the current account state is
hashed and compared. A mismatch produces `AccountVersionMismatch`,
preventing TOCTOU races where an account is mutated between load and apply.

The saga `commit()` path auto-populates snapshots when none are provided.

## Subaccounts

An account is identified by a base id plus an `i64` **subaccount**, written
`AccountId { id, sub }`; `sub = 0` is the main account. Each `(id,
sub)` is its own record with its own policy, flags, book, and version, so a
subaccount is a full account that happens to share a base id. Subaccounts are how
one account holds many concurrent inflights: an inflight hold is a subaccount of
its destination, keyed by a value derived from the trade (see
[adr/0012-subaccounts.md](adr/0012-subaccounts.md)).

Balances are reported **segregated per subaccount**, never summed across them:

- `balance(&AccountId, asset)` reads one subaccount.
- `balances(&AccountId, asset, sub)` returns one entry per non-closed subaccount
  (`sub = None` spans all; `Some(s)` filters to one). Closed subaccounts are
  excluded.
- `list_subaccounts(&AccountId)` lists the non-closed subaccounts of a base
  account.

## Balance Computation

Balance for an (account reference, asset) pair is computed as:

```
balance(account, asset) = sum(p.value for p in postings
                              where p.owner == account
                              and   p.asset == asset
                              and   p.status != Inactive)
```

There is no stored balance field. This eliminates drift between the balance
and the underlying postings.

## Account Types in Practice

### Regular user accounts (`NoOverdraft`)

Hold positive postings only. Cannot go negative. Used for end-user wallets,
merchant accounts, etc.

### System accounts (`SystemAccount`)

Operational accounts representing issuance, sink, revenue, COGS, fees, or
internal balancing. Can hold negative postings (offset positions, e.g. a
liability when the account is the deposit counterparty). Used as the
counterparty in deposits: the system account takes on a negative balance to
offset the value credited elsewhere.

### External accounts (`ExternalAccount`)

Boundary accounts representing the outside world (banks, payment
processors). They represent value entering and leaving the ledger boundary,
and like system accounts they can hold negative postings (offset positions).

### Credit accounts (`CappedOverdraft`)

Accounts with a negative floor (e.g. credit lines). The floor is the maximum
allowed overdraft. When the account's positive postings are insufficient for
a debit, a negative posting is created to cover the shortfall, down to the
floor. The floor is re-validated as the last step before finalize and is
best-effort under concurrency (see above).
