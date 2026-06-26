# Glossary & Usage Guide

## Terms

### Posting

A signed amount of one asset owned by one account. The fundamental unit of value in the ledger. Postings are immutable once created — consumed postings are marked `Inactive` but never deleted.

- **Positive posting**: a holding (the account owns value).
- **Negative posting**: a liability (only allowed on `SystemAccount` and `ExternalAccount`).

Lifecycle: `Active` → `PendingInactive` (reserved by saga) → `Inactive` (consumed).

### Account

A versioned entity that owns postings. Balance is never stored — it is always the sum of non-inactive postings for a given (account, asset) pair.

Accounts have a **policy** (balance floor rule), **flags** (lifecycle + user-defined), and a **journal** assignment.

### Asset

An identifier (`AssetId(u32)`) representing a unit of value — a currency, a product, a token. Each asset is an independent conservation boundary: the sum of consumed postings must equal the sum of created postings *per asset* in every transfer.

### Movement

The intent layer's building block: `{ from, to, asset, amount }`. Movements express *what* should happen. The ledger resolves them into concrete postings.

### Transfer

One or more movements to execute atomically. Built via `TransferBuilder`, committed via `ledger.commit(transfer)`.

### Envelope

The resolved, concrete form of a transfer: which postings to consume and which to create. Produced internally by the resolve step. Available for direct use via `commit_atomic(envelope)`.

### Journal

A named scope that controls which accounts and assets may participate in transfers. Journals do **not** partition balances — accounts and their balances are global. Journals only gate *who can transact with whom in what context*.

A journal has:
- `allowed_assets` — if non-empty, only these assets may appear in movements.
- `allowed_flags` — if non-empty, accounts with ANY of these flags may participate.
- `allowed_accounts` — if non-empty, these specific accounts may participate (in addition to flag matches).

An empty journal (no restrictions) allows any account and any asset.

### Conservation

For every transfer, for each asset: `sum(consumed) == sum(created)`. This is the double-entry bookkeeping invariant, enforced at the type level. No value is created or destroyed — it only moves.

### AutoId

Snowflake-inspired `i64` identifier: `[0 sign bit][40-bit ms timestamp][23-bit counter or CRC32]`. Generated in Rust — the database never assigns IDs.

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

// Journals — separate deposit/withdrawal flows from trading
let deposits_journal = JournalBuilder::new("deposits")
    .allow_asset(usd)
    .allow_asset(eur)
    .allow_flags(AccountFlags::USER_0 | AccountFlags::USER_1) // wallets + bank
    .build();

let trading_journal = JournalBuilder::new("trading")
    .allow_asset(usd)
    .allow_asset(eur)
    .allow_flags(AccountFlags::USER_0) // only user wallets
    .allow_account(exchange_pool)       // + the exchange pool
    .build();

ledger.create_journal(deposits_journal).await?;
ledger.create_journal(trading_journal).await?;

// Accounts
let bank = Account {
    id: AccountId::default(),
    policy: AccountPolicy::ExternalAccount,
    flags: AccountFlags::USER_1, // bank flag
    journal: deposits_journal.id,
    ..Default::default()
};

let alice = Account {
    id: AccountId::default(),
    policy: AccountPolicy::NoOverdraft,
    flags: AccountFlags::USER_0, // wallet flag
    journal: deposits_journal.id,
    ..Default::default()
};

let exchange_pool = Account {
    id: AccountId::default(),
    policy: AccountPolicy::SystemAccount,
    flags: AccountFlags::empty(),
    journal: trading_journal.id,
    ..Default::default()
};
```

**Deposit USD into Alice's wallet:**

```rust
let deposit = TransferBuilder::new()
    .journal(deposits_journal.id)
    .deposit(alice.id, usd, Cent::from(10_000), bank.id)?
    .build();
ledger.commit(deposit).await?;
// Alice: +10,000 USD
// Bank: -10,000 USD (liability — money entered the system)
```

**Alice trades 5,000 USD for EUR at 1:0.92:**

```rust
let trade = TransferBuilder::new()
    .journal(trading_journal.id)
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
    .journal(deposits_journal.id)
    .withdraw(alice.id, eur, Cent::from(4_600), bank.id)
    .build();
ledger.commit(withdrawal).await?;
// Alice: 5,000 USD, 0 EUR
// Bank: -10,000 USD + 4,600 EUR
```

Conservation holds at every step. The exchange pool absorbs the spread.


### Example 2: Supermarket / Retail POS

A supermarket tracks inventory as product assets, records sales with COGS, and manages cash and bank accounts.

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

// Journals
let sales_journal = JournalBuilder::new("sales")
    .allow_asset(gs)
    .allow_asset(product_a)
    .allow_asset(product_b)
    .allow_flags(WAREHOUSE | CUSTOMER | REVENUE)
    .build();

let inventory_journal = JournalBuilder::new("inventory")
    .allow_asset(product_a)
    .allow_asset(product_b)
    .allow_flags(WAREHOUSE)
    .allow_account(world) // issuance source
    .build();

let banking_journal = JournalBuilder::new("banking")
    .allow_asset(gs)
    .allow_flags(WAREHOUSE | BANK)
    .build();

// Accounts
let world = Account {  // issuance source — mints product tokens on receipt
    policy: AccountPolicy::SystemAccount,
    flags: AccountFlags::empty(),
    ..Default::default()
};

let warehouse = Account {
    policy: AccountPolicy::NoOverdraft,
    flags: WAREHOUSE,
    ..Default::default()
};

let cash_register = Account {
    policy: AccountPolicy::NoOverdraft,
    flags: WAREHOUSE,
    ..Default::default()
};

let revenue = Account {
    policy: AccountPolicy::SystemAccount,
    flags: REVENUE,
    ..Default::default()
};

let cogs = Account {  // cost of goods sold
    policy: AccountPolicy::SystemAccount,
    flags: REVENUE,
    ..Default::default()
};

let bank = Account {
    policy: AccountPolicy::NoOverdraft,
    flags: BANK,
    ..Default::default()
};
```

**Receive inventory from supplier (50 units of rice):**

```rust
let receipt = TransferBuilder::new()
    .journal(inventory_journal.id)
    .pay(world, warehouse.id, product_a, Cent::from(50_000)) // 50.000 units (precision 3)
    .build();
ledger.commit(receipt).await?;
// Warehouse: +50.000 rice
// World: -50.000 rice (liability — issued into system)
```

**Cash sale — customer buys 2 rice at 15,000 Gs each:**

```rust
let sale = TransferBuilder::new()
    .journal(sales_journal.id)
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
    .journal(banking_journal.id)
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
// 20,000 Gs — gross profit = revenue - cogs = 10,000 Gs
```

**Why journals matter here:** The `sales` journal prevents a bug where a bank transfer accidentally credits the revenue account. The `banking` journal ensures only cash and bank accounts participate in deposits. Each flow is isolated by scope while sharing the same global balances.

---

## Journal Design

### When to use journals

- **Always** — even if you only have one flow, defining a journal documents what assets and accounts are expected.
- **Multiple flows** — separate journals for sales, payments, inventory, banking. Prevents cross-contamination.
- **Multi-tenant** — one journal per tenant with `allowed_accounts` restricting to that tenant's accounts.

### Journal scoping rules

| Field | Empty | Non-empty |
|-------|-------|-----------|
| `allowed_assets` | Any asset allowed | Only listed assets |
| `allowed_flags` | Flag check skipped | Accounts with ANY matching flag pass |
| `allowed_accounts` | Account check skipped | Listed accounts always pass (even without matching flags) |

An account passes the journal check if:
1. It matches `allowed_flags` (any flag in common), OR
2. It is explicitly listed in `allowed_accounts`, OR
3. Both lists are empty (unrestricted journal).

### Journals do NOT partition balances

An account's balance is the sum of all its non-inactive postings across ALL journals. If Alice receives 100 USD via the `deposits` journal and spends 50 USD via the `trading` journal, her balance is 50 USD — not 100 in one journal and -50 in another.

This is intentional: journals scope *access*, not *state*.
