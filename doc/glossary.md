# Glossary & Usage Guide

> Coming from classical accounting? See
> [accounting-mapping.md](accounting-mapping.md) for how journals, entries, and
> ledgers map onto Kuatia's transfers, postings, and books.

## Terms

### Posting

A signed amount of one asset owned by one account. The fundamental unit of
value in the ledger. Postings are immutable once created. Consumed postings
are marked `Inactive` but never deleted.

- **Positive posting**: value controlled by the account.
- **Negative posting**: an offset position, allowed on any policy except
  `NoOverdraft`. It represents issuance, external flow, system balancing
  (`SystemAccount`, `ExternalAccount`), or an overdraft
  (`CappedOverdraft`/`UncappedOverdraft`).

Lifecycle: `Active` → `PendingInactive` (reserved by a saga, stamped with its
`ReservationId`) → `Inactive` (consumed). **Ledger balance** sums
`Active + PendingInactive` postings; **available balance** sums only `Active`
(postings reserved for an in-flight transfer are not available to spend).

### Account

A versioned entity that owns postings. Balance is never stored. It is always
the sum of non-inactive postings for a given (account, asset) pair.

Accounts have a **policy** (balance floor rule), **flags** (lifecycle +
user-defined), and a **book** assignment.

### Asset

An identifier (`AssetId(u32)`) representing a unit of value: a currency, a
product, a token. Each asset is an independent conservation boundary: the sum
of consumed postings must equal the sum of created postings *per asset* in
every transfer.

### Movement

The intent layer's building block: `{ from, to, asset, amount }`. Movements
express *what* should happen. The ledger resolves them into concrete postings.

### Transfer

One or more movements to execute atomically. Built via `TransferBuilder`,
committed via `ledger.commit(transfer)`.

### Envelope

The resolved, concrete form of a transfer: which postings to consume and which
to create. Produced by the resolve step (`commit`), or built directly and
committed via `commit_envelope(envelope)`.

### Dumb storage

The design where every `Store` write method applies one update and returns the
**number of affected rows** (or an I/O error), never interpreting that count,
deciding state, enforcing idempotency, or compensating. The saga reads the
count and decides: full = continue; partial = error → compensate; zero = read
state and continue only if this same envelope/reservation already applied it.

### Reservation protocol

The concurrency-control mechanism for consumed postings: `reserve_postings`
atomically flips `Active → PendingInactive` stamped with a `ReservationId`,
so two sagas cannot both claim the same posting. This (not a global
transaction) is what prevents double-spend.

### PendingSaga / recovery

A write-ahead record `{envelope, reservation, phase}` persisted via
`SagaStore` before a commit mutates anything. The `phase`
(`Reserving` → `Finalizing`) tells `Ledger::recover()` (startup) how to
complete a crashed saga: a `Reserving` saga is re-run and **re-validated**;
a `Finalizing` saga (already validated, owns its postings) is rolled forward
through the verified `finalize_envelope`. Roll-forward, not rollback.

### Book

A **Book is a transfer policy scope**: it gates which accounts and assets may
participate in a transfer. Note what it is *not*:

- It is **not** the classical accounting journal (the chronological book of
  entries). That role is played by the append-only transfer log itself.
- It does **not** partition balances. Accounts and their balances are global;
  a Book only gates *who can transact with whom in what context*.

A book is `{ id, name, policy }`, where the `policy` (`BookPolicy`) holds:
- `allowed_assets`: if non-empty, only these assets may appear in movements.
- `allowed_flags`: if non-empty, accounts with ANY of these flags may
  participate.
- `allowed_accounts`: if non-empty, these specific accounts may participate
  (in addition to flag matches).

An empty policy (no restrictions) allows any account and any asset.

### Conservation

For every transfer, for each asset: `sum(consumed) == sum(created)`. This is
the double-entry-style safety invariant (the UTXO-model equivalent of
`Σ debits = Σ credits`), enforced at the type level. No value is created or
destroyed. It only moves.

### AutoId

Snowflake-inspired `i64` identifier:
`[0 sign bit][40-bit ms timestamp][23-bit counter or CRC32]`. The timestamp
counts milliseconds since `KUATIA_EPOCH_MS` (2026-01-01T00:00:00Z), giving
~34.8 years of range going forward. Generated in Rust. The database never
assigns IDs.

---

## Usage Examples

### Example 1: Currency Exchange

An exchange lets users deposit fiat, trade between currencies, and withdraw.

**Setup:**

```rust
use kuatia::prelude::*;

// Assets
let usd = AssetId::new(1);
let eur = AssetId::new(2);

// Books: separate deposit/withdrawal flows from trading
let deposits_book = BookBuilder::new("deposits")
    .allow_asset(usd)
    .allow_asset(eur)
    .allow_flags(AccountFlags::USER_0 | AccountFlags::USER_1) // wallets + bank
    .build();

let trading_book = BookBuilder::new("trading")
    .allow_asset(usd)
    .allow_asset(eur)
    .allow_flags(AccountFlags::USER_0) // only user wallets
    .allow_account(exchange_pool)       // + the exchange pool
    .build();

ledger.create_book(deposits_book).await?;
ledger.create_book(trading_book).await?;

// Accounts — `Account::new` sets version 1, no flags, and the default book;
// set the other fields explicitly where the common case is not enough.
let mut bank = Account::new(AccountId::default(), AccountPolicy::ExternalAccount);
bank.flags = AccountFlags::USER_1; // bank flag
bank.book = deposits_book.id;

let mut alice = Account::new(AccountId::default(), AccountPolicy::NoOverdraft);
alice.flags = AccountFlags::USER_0; // wallet flag
alice.book = deposits_book.id;

let mut exchange_pool = Account::new(AccountId::default(), AccountPolicy::SystemAccount);
exchange_pool.book = trading_book.id;
```

**Deposit USD into Alice's wallet:**

```rust
let deposit = TransferBuilder::new()
    .book(deposits_book.id)
    .deposit(alice.id, usd, Cent::from(10_000), bank.id)?
    .build();
ledger.commit(deposit).await?;
// Alice: +10,000 USD
// Bank: -10,000 USD (offset: value entered the ledger boundary)
```

**Alice trades 5,000 USD for EUR at 1:0.92:**

```rust
let trade = TransferBuilder::new()
    .book(trading_book.id)
    .pay(alice.id, exchange_pool, usd, Cent::from(5_000))
    .pay(exchange_pool, alice.id, eur, Cent::from(4_600))
    .build();
ledger.commit(trade).await?;
// Alice: 5,000 USD + 4,600 EUR
// Exchange pool: 5,000 USD - 4,600 EUR
```

**Withdraw EUR to Alice's bank:**

```rust
let withdrawal = TransferBuilder::new()
    .book(deposits_book.id)
    .withdraw(alice.id, eur, Cent::from(4_600), bank.id)
    .build();
ledger.commit(withdrawal).await?;
// Alice: 5,000 USD, 0 EUR
// Bank: -10,000 USD + 4,600 EUR
```

Conservation holds at every step. The exchange pool absorbs the spread.


### Example 2: Supermarket / Retail POS

A supermarket tracks inventory as product assets, records sales with COGS, and
manages cash and bank accounts.

**Setup:**

```rust
// Assets
let gs = AssetId::new(1);            // Guaranies (currency)
let product_a = AssetId::new(100);   // Product: rice 1kg
let product_b = AssetId::new(101);   // Product: cooking oil 1L

// Account flags
const WAREHOUSE: AccountFlags = AccountFlags::USER_0;
const CUSTOMER: AccountFlags = AccountFlags::USER_1;
const REVENUE: AccountFlags = AccountFlags::USER_2;
const BANK: AccountFlags = AccountFlags::USER_3;

// Books
let sales_book = BookBuilder::new("sales")
    .allow_asset(gs)
    .allow_asset(product_a)
    .allow_asset(product_b)
    .allow_flags(WAREHOUSE | CUSTOMER | REVENUE)
    .build();

let inventory_book = BookBuilder::new("inventory")
    .allow_asset(product_a)
    .allow_asset(product_b)
    .allow_flags(WAREHOUSE)
    .allow_account(world) // issuance source
    .build();

let banking_book = BookBuilder::new("banking")
    .allow_asset(gs)
    .allow_flags(WAREHOUSE | BANK)
    .build();

// Accounts — start from `Account::new`, then set flags where needed.
// issuance source: mints product tokens on receipt
let world = Account::new(AccountId::default(), AccountPolicy::SystemAccount);

let mut warehouse = Account::new(AccountId::default(), AccountPolicy::NoOverdraft);
warehouse.flags = WAREHOUSE;

let mut cash_register = Account::new(AccountId::default(), AccountPolicy::NoOverdraft);
cash_register.flags = WAREHOUSE;

let mut revenue = Account::new(AccountId::default(), AccountPolicy::SystemAccount);
revenue.flags = REVENUE;

let mut cogs = Account::new(AccountId::default(), AccountPolicy::SystemAccount); // cost of goods sold
cogs.flags = REVENUE;

let mut bank = Account::new(AccountId::default(), AccountPolicy::NoOverdraft);
bank.flags = BANK;
```

**Receive inventory from supplier (50 units of rice):**

```rust
let receipt = TransferBuilder::new()
    .book(inventory_book.id)
    .pay(world, warehouse.id, product_a, Cent::from(50_000)) // 50.000 units (precision 3)
    .build();
ledger.commit(receipt).await?;
// Warehouse: +50.000 rice
// World: -50.000 rice (offset: issued into the ledger)
```

**Cash sale, customer buys 2 rice at 15,000 Gs each:**

```rust
let sale = TransferBuilder::new()
    .book(sales_book.id)
    // Move product from warehouse to customer (consumed by sale)
    .pay(warehouse.id, customer.id, product_a, Cent::from(2_000))
    // Customer pays cash
    .pay(customer.id, cash_register.id, gs, Cent::from(30_000))
    // Record revenue
    .pay(world, revenue.id, gs, Cent::from(30_000))
    // Record COGS (cost was 10,000 Gs per unit)
    .pay(world, cogs.id, gs, Cent::from(20_000))
    .build();
ledger.commit(sale).await?;
```

**Deposit cash to bank:**

```rust
let deposit = TransferBuilder::new()
    .book(banking_book.id)
    .pay(cash_register.id, bank.id, gs, Cent::from(30_000))
    .build();
ledger.commit(deposit).await?;
```

**Query balances:**

```rust
let warehouse_rice = ledger.balance(&warehouse.id, &product_a).await?;
// 48.000 units remaining

let bank_balance = ledger.balance(&bank.id, &gs).await?;
// 30,000 Gs

let total_revenue = ledger.balance(&revenue.id, &gs).await?;
// 30,000 Gs

let total_cogs = ledger.balance(&cogs.id, &gs).await?;
// 20,000 Gs gross profit = revenue - cogs = 10,000 Gs
```

**Why books matter here:** The `sales` book prevents a bug where a bank
transfer accidentally credits the revenue account. The `banking` book ensures
only cash and bank accounts participate in deposits. Each flow is isolated by
scope while sharing the same global balances.

---

## Book Design

### When to use books

- **Always**: even if you only have one flow, defining a book documents what
  assets and accounts are expected.
- **Multiple flows**: separate books for sales, payments, inventory, banking.
  Prevents cross-contamination.
- **Multi-tenant**: one book per tenant with `allowed_accounts` restricting to
  that tenant's accounts.

### Book scoping rules

| Field | Empty | Non-empty |
|-------|-------|-----------|
| `allowed_assets` | Any asset allowed | Only listed assets |
| `allowed_flags` | Flag check skipped | Accounts with ANY matching flag pass |
| `allowed_accounts` | Account check skipped | Listed accounts always pass (even without matching flags) |

An account passes the book check if:
1. It matches `allowed_flags` (any flag in common), OR
2. It is explicitly listed in `allowed_accounts`, OR
3. Both lists are empty (unrestricted book).

### Books do NOT partition balances

An account's balance is the sum of all its non-inactive postings across ALL
books. If Alice receives 100 USD via the `deposits` book and spends 50 USD via
the `trading` book, her balance is 50 USD, not 100 in one book and -50 in
another.

This is intentional: books scope *access*, not *state*.
