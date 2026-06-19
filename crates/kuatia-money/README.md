# kuatia-money

Monetary amounts for the kuatia ledger.

`Cent` is a signed amount in an asset's smallest unit. It wraps an integer
whose width is an internal detail: the public API never names the backing type,
and no serialized form reveals it. The width is chosen once at compile time
through the `Backing` alias, which defaults to `i64` and switches to `i128`
under the `i128` cargo feature.

All arithmetic is checked. Addition, subtraction, and negation return
`OverflowError` instead of wrapping, so the ledger's per-asset conservation sum
can never silently round or overflow.

## Key types

| Type | Description |
|------|-------------|
| `Cent` | Signed amount in an asset's smallest unit, checked arithmetic |
| `Backing` | The integer type behind every `Cent` (`i64`, or `i128` via feature) |
| `CentBacking` | Trait an integer must satisfy to back a `Cent` |
| `OverflowError` | Returned when a checked operation would overflow |

## Features

- `i128` — swap the backing integer from `i64` to `i128` across the whole
  dependency chain.
