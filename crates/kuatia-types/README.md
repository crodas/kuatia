# kuatia-types

Domain types for the kuatia ledger.

Pure data structures with no IO, no async, and minimal dependencies (`serde`, `bitflags`).
This crate is the foundation — every other kuatia crate depends on it.

## Key types

| Type | Description |
|------|-------------|
| `AccountId(i64)` | Stable account identity |
| `AssetId(u32)` | Asset identifier — conservation boundary |
| `EnvelopeId([u8; 32])` | Content-addressed transfer hash |
| `PostingId { transfer, index }` | Posting identity within a transfer |
| `Cent(i64)` | Smallest monetary unit, checked arithmetic |
| `Posting` | Signed amount owned by one account (positive = held, negative = offset) |
| `Transfer` | Atomic unit: consumes + creates postings |
| `Account` | Versioned entity with policy, flags, and book |
| `Book` / `BookId` | Transfer policy scope — gates which accounts/assets may participate |
| `PostingStatus` | `Active` → `PendingInactive` → `Inactive` |

## Traits

- **`ToBytes`** — deterministic binary serialization for content-addressing
