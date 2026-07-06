//! Demo data. Builds an in-memory ledger with a handful of accounts, funds
//! them from an external boundary account, then runs payments and a
//! multi-asset trade so the dashboard has something to visualize.

use std::sync::Arc;

use kuatia::ledger::Ledger;
use kuatia_core::{Account, AccountId, AccountPolicy, Amount, AssetId, Cent, TransferBuilder};
use kuatia_storage_sql::SqlStore;

use crate::assets::{BTC, EUR, USD};

/// Well-known account ids used by the demo.
pub const TREASURY: AccountId = AccountId::new(1);
pub const EXTERNAL: AccountId = AccountId::new(99);
pub const ALICE: AccountId = AccountId::new(100);
/// A subaccount of Alice: an earmarked savings bucket under the same base id,
/// with its own balance that is never summed into Alice's main account.
pub const ALICE_SAVINGS: AccountId = AccountId::with_sub(100, 1);
pub const BOB: AccountId = AccountId::new(101);
pub const CAROL: AccountId = AccountId::new(102);
pub const MERCHANT: AccountId = AccountId::new(103);

/// Human-readable labels for the seeded accounts, surfaced by the API so the
/// frontend can show names instead of raw ids.
pub fn account_label(id: AccountId) -> Option<&'static str> {
    Some(match id {
        TREASURY => "Treasury",
        EXTERNAL => "External",
        ALICE => "Alice",
        ALICE_SAVINGS => "Alice / Savings",
        BOB => "Bob",
        CAROL => "Carol",
        MERCHANT => "Merchant",
        _ => return None,
    })
}

/// Connect to the ledger database at `db_url`, create the schema, and run
/// recovery. The URL scheme selects the backend (e.g. `sqlite::memory:`,
/// `sqlite://kuatia.db`, `postgres://user:pass@host/db`).
///
/// The pool is capped at a single connection: `sqlite::memory:` gives each
/// connection its own separate database, so more than one would split the
/// ledger; one connection is also fine for a low-traffic dashboard on a file or
/// Postgres backend.
pub async fn connect(db_url: &str) -> Result<Arc<Ledger>, Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();
    let pool = sqlx::any::AnyPoolOptions::new()
        .max_connections(1)
        .connect(&sqlite_creatable(db_url))
        .await?;
    let store = SqlStore::new(pool);
    store.migrate().await?;
    let ledger = Arc::new(Ledger::new(store));
    ledger.recover().await?;
    Ok(ledger)
}

/// A SQLite backend will not create a missing file unless the URL asks for it,
/// so add `mode=rwc` to a file-backed `sqlite:` URL that does not already set a
/// mode. In-memory and non-SQLite URLs pass through unchanged.
fn sqlite_creatable(db_url: &str) -> String {
    if !db_url.starts_with("sqlite:") || db_url.contains(":memory:") || db_url.contains("mode=") {
        return db_url.to_string();
    }
    let sep = if db_url.contains('?') { '&' } else { '?' };
    format!("{db_url}{sep}mode=rwc")
}

/// Seed the demo data only if the ledger has no accounts yet. Returns `true` if
/// it seeded, `false` if the ledger was already populated (so re-running with
/// `--seed` against a persistent database is a safe no-op rather than a
/// duplicate-id error).
pub async fn seed_if_empty(ledger: &Arc<Ledger>) -> Result<bool, Box<dyn std::error::Error>> {
    if !ledger.list_accounts().await?.is_empty() {
        return Ok(false);
    }
    populate(ledger).await?;
    Ok(true)
}

/// Populate the ledger with demo accounts and a spread of transfers.
pub async fn populate(ledger: &Arc<Ledger>) -> Result<(), Box<dyn std::error::Error>> {
    // Two-decimal assets (USD, EUR) and an 8-decimal asset (BTC).
    let fiat = Amount::new(2);
    let btc = Amount::new(8);

    create(ledger, TREASURY, AccountPolicy::SystemAccount).await?;
    create(ledger, EXTERNAL, AccountPolicy::ExternalAccount).await?;
    create(ledger, ALICE, AccountPolicy::NoOverdraft).await?;
    create(ledger, ALICE_SAVINGS, AccountPolicy::NoOverdraft).await?;
    create(ledger, BOB, AccountPolicy::NoOverdraft).await?;
    // Carol may overdraw down to -$500.00.
    create(
        ledger,
        CAROL,
        AccountPolicy::CappedOverdraft {
            floor: fiat.parse("-500.00")?,
        },
    )
    .await?;
    create(ledger, MERCHANT, AccountPolicy::NoOverdraft).await?;

    // Fund accounts from the external boundary.
    deposit(ledger, ALICE, USD, fiat.parse("1000.00")?).await?;
    deposit(ledger, BOB, EUR, fiat.parse("500.00")?).await?;
    deposit(ledger, ALICE, BTC, btc.parse("0.50000000")?).await?;
    deposit(ledger, CAROL, USD, fiat.parse("200.00")?).await?;

    // Ordinary payments between held balances.
    pay(ledger, ALICE, BOB, USD, fiat.parse("150.00")?).await?;
    pay(ledger, BOB, MERCHANT, EUR, fiat.parse("80.00")?).await?;
    pay(ledger, ALICE, MERCHANT, BTC, btc.parse("0.10000000")?).await?;

    // Carol spends past her balance, into the capped overdraft.
    pay(ledger, CAROL, MERCHANT, USD, fiat.parse("250.00")?).await?;

    // Alice earmarks part of her balance into her savings subaccount. The two
    // balances stay segregated under the same base id.
    pay(ledger, ALICE, ALICE_SAVINGS, USD, fiat.parse("300.00")?).await?;

    // Atomic multi-asset trade: Alice buys EUR from Bob with USD.
    let trade = TransferBuilder::new()
        .pay(ALICE, BOB, USD, fiat.parse("100.00")?)
        .pay(BOB, ALICE, EUR, fiat.parse("90.00")?)
        .build();
    ledger.commit(trade).await?;

    Ok(())
}

async fn create(
    ledger: &Arc<Ledger>,
    id: AccountId,
    policy: AccountPolicy,
) -> Result<(), Box<dyn std::error::Error>> {
    ledger.create_account(Account::new(id, policy)).await?;
    Ok(())
}

async fn deposit(
    ledger: &Arc<Ledger>,
    to: AccountId,
    asset: AssetId,
    amount: Cent,
) -> Result<(), Box<dyn std::error::Error>> {
    let transfer = TransferBuilder::new()
        .deposit(to, asset, amount, EXTERNAL)?
        .build();
    ledger.commit(transfer).await?;
    Ok(())
}

async fn pay(
    ledger: &Arc<Ledger>,
    from: AccountId,
    to: AccountId,
    asset: AssetId,
    amount: Cent,
) -> Result<(), Box<dyn std::error::Error>> {
    let transfer = TransferBuilder::new().pay(from, to, asset, amount).build();
    ledger.commit(transfer).await?;
    Ok(())
}
