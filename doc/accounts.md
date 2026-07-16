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
| `flags` | `AccountFlags` | Lifecycle (`FROZEN`, `CLOSED`, `INFLIGHT`), the balance constraint (`DEBIT_MUST_NOT_EXCEED_CREDIT`), and user-defined bits |
| `book` | `BookId` | Book this account belongs to |
| `metadata` | `Metadata` | `BTreeMap<String, Vec<u8>>` for free-form data |

## Subaccounts

An `AccountId` is a base `id` plus a `sub`. `sub = 0` is the account's main
account; a non-zero `sub` is a subaccount of the same base id. Each `(id, sub)`
is a full account record with its own flags, book, version, and
lifecycle, created, versioned, frozen, and closed exactly like any other
account. A subaccount can forbid overdraft while its base account does not, or
the reverse, because every check keys on the full `AccountId`.

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

## Balance constraint

An account carries one balance rule, held in its flags:

| State | Balance floor | Negative postings |
|-------|--------------|-------------------|
| default (no flag) | none | yes (overdraft allowed, unbounded) |
| `DEBIT_MUST_NOT_EXCEED_CREDIT` | `>= 0` | no |

By default an account may overdraw without bound. An overdraft is a negative
posting (an offset position) assigned to the account to cover a shortfall: when
the account's positive postings are insufficient for a debit, the resolve step
consumes them all and creates a negative posting for the remainder. The transfer
is recorded as long as it conserves value per asset.

Setting `DEBIT_MUST_NOT_EXCEED_CREDIT` forbids this: the account's debits may
never exceed its credits, so its balance may not go negative and it may not hold
a negative posting. Validation rejects any transfer that would create a negative
posting on such an account or project its balance below zero. The convenience
constructor `Account::debit_must_not_exceed_credit(id)` names the invariant
directly, and `Account::forbids_overdraft()` reports it. There is no bounded
"credit line" floor between the two: a credit-line limit, if needed, is enforced
by the application above the ledger.

The zero-floor check is re-validated as the last step before finalize writes
(the finalize step re-loads balances and account versions and re-runs validation
just before deactivating). Double-spend safety is exact regardless: the
reservation protocol (an atomic conditional `reserve_postings`) guarantees a
posting cannot be consumed twice. See
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
Version 1: { flags: DEBIT_MUST_NOT_EXCEED_CREDIT }          ← created
Version 2: { flags: DEBIT_MUST_NOT_EXCEED_CREDIT | FROZEN } ← frozen
Version 3: { flags: DEBIT_MUST_NOT_EXCEED_CREDIT }          ← unfrozen
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
sub)` is its own record with its own flags, book, and version, so a
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

The single flag divides accounts into two kinds. Intent (wallet vs. boundary
vs. system) is a matter of how you use the account, not a distinct type.

### Overdraft-forbidding accounts (`DEBIT_MUST_NOT_EXCEED_CREDIT`)

Hold positive postings only. Cannot go negative. Used for end-user wallets,
merchant accounts, and any account that must never spend value it does not hold.
Construct with `Account::debit_must_not_exceed_credit(id)`.

### Overdraft-permitting accounts (default)

An ordinary `Account::new(id)` may go negative without bound. This covers:

- **Issuance / system balancing**: revenue, COGS, fees, or internal balancing
  accounts that hold offset positions.
- **Ledger boundary**: the counterparty in deposits and withdrawals, where value
  enters or leaves the ledger. It takes on a negative balance to offset the value
  credited elsewhere; this is normal, not a bug.
- **Credit-like accounts**: where the balance is allowed below zero. The ledger
  enforces no upper bound on the overdraft; a specific credit limit is the
  application's responsibility.

When such an account's positive postings are insufficient for a debit, a negative
posting covers the shortfall.
