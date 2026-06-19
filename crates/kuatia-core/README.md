# kuatia-core

Pure, sans-IO decision logic for the kuatia ledger.

No async runtime, no IO, near-zero dependencies. Deterministic and testable
with golden vectors. Depends only on `kuatia-types` and `sha2`.

## Modules

| Module | Purpose |
|--------|---------|
| `validate` | `validate_and_plan()` — single entry point for all invariant checks |
| `hash` | Double-SHA256, content-addressed transfer IDs, account snapshot hashing |
| `posting_selection` | Greedy largest-first posting selection for the intent layer |

## Validation invariants

`validate_and_plan()` checks, in order:

1. Non-empty transfer
2. No duplicate consumed postings
3. All consumed postings exist
4. All consumed postings are Active or PendingInactive
5. All accounts exist, not frozen, not closed
6. Account snapshot pinning (OCC)
7. Book policy (if a book is loaded): referenced assets/accounts/flags allowed
8. Per-asset conservation: `sum(consumed) == sum(created)`
9. Negative postings forbidden only on `NoOverdraft` (allowed on overdraft/system/external)
10. Account policy enforcement (overdraft limits)

Returns a `Plan` on success, or a `ValidationError` describing the violation.
