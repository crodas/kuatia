# Accounts

## Overview

An account is a versioned entity that owns postings. Balance is never stored — it is always computed as the sum of active postings for a given (account, asset) pair.

## Structure

| Field | Type | Description |
|-------|------|-------------|
| `id` | `AccountId(i64)` | Stable identity, assigned at creation |
| `version` | `u64` | Starts at 1, increments on every mutation |
| `policy` | `AccountPolicy` | Balance floor rule (see below) |
| `flags` | `AccountFlags` | Lifecycle flags: `FROZEN`, `CLOSED` |
| `book` | `u32` | Grouping label (e.g. tenant or product line) |
| `code` | `u32` | Category code (e.g. chart of accounts) |
| `user_data` | `UserData` | Fixed 28 bytes: `u128 + u64 + u32` for external refs |
| `metadata` | `Metadata` | `BTreeMap<String, Vec<u8>>` for free-form data |

## Policies

Each account has a policy that controls what balance constraints apply:

| Policy | Balance floor | Negative postings | CAS guard |
|--------|--------------|-------------------|-----------|
| `NoOverdraft` | `>= 0` | No | No |
| `CappedOverdraft { floor }` | `>= floor` | No | Yes |
| `UncappedOverdraft` | None | No | No |
| `SystemAccount` | None | Yes | No |
| `ExternalAccount` | None | Yes | No |

Only `SystemAccount` and `ExternalAccount` may hold negative postings (liabilities). Validation rejects any transfer that would create a negative posting on another account type.

`CappedOverdraft` accounts emit CAS (Compare-And-Swap) guards during validation to prevent write-skew — two concurrent transfers could each pass validation independently but together push the balance below the floor.

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

The store enforces `version_new == version_current + 1`, preventing gaps or overwrites. The full history is queryable via `account_history(id)`.

## Snapshot Pinning

Transfers can carry `AccountSnapshotId` values — pairs of `(AccountId, snapshot_hash)` recording which account version the transfer was validated against.

During validation, if snapshots are present, the current account state is hashed and compared. A mismatch produces `AccountVersionMismatch`, preventing TOCTOU races where an account is mutated between load and apply.

The saga `commit()` path auto-populates snapshots when none are provided.

## Balance Computation

Balance for an (account, asset) pair is computed as:

```
balance(account, asset) = sum(p.value for p in postings
                              where p.owner == account
                              and   p.asset == asset
                              and   p.status != Inactive)
```

There is no stored balance field. This eliminates drift between the balance and the underlying postings.

## Account Types in Practice

### Regular user accounts (`NoOverdraft`)

Hold positive postings only. Cannot go negative. Used for end-user wallets, merchant accounts, etc.

### System accounts (`SystemAccount`)

Operational accounts for fees, settlement, market-making. Can hold negative postings (liabilities). Used as the counterparty in deposits — the system account takes on a negative balance to represent the liability.

### External accounts (`ExternalAccount`)

Boundary accounts representing entities outside the ledger (banks, payment processors). Like system accounts, they can hold negative postings. Used to track money entering and leaving the system.

### Credit accounts (`CappedOverdraft`)

Accounts with a negative floor (e.g. credit lines). The floor is the maximum allowed overdraft. Write-skew prevention via CAS guards ensures concurrent transfers respect the floor.
