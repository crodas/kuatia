//! Fund two accounts with different assets, then trade between them atomically.
//!
//! Run with:
//! ```sh
//! cargo run -p kuatia --example fund_and_trade
//! ```

use std::sync::Arc;

use kuatia::ledger::Ledger;
use kuatia_core::*;
use kuatia_storage_sql::SqlStore;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ledger = connect().await?;

    let alice = AccountId::new(1);
    let bob = AccountId::new(2);
    let external = AccountId::new(99);
    let usd = AssetId::new(1);
    let eur = AssetId::new(2);

    // Two-decimal money: `money.parse("100.00")` -> Cent in minor units.
    let money = Amount::new(2);

    ledger
        .create_account(Account::debit_must_not_exceed_credit(alice))
        .await?;
    ledger
        .create_account(Account::debit_must_not_exceed_credit(bob))
        .await?;
    ledger.create_account(Account::new(external)).await?;

    // Fund: $100.00 to Alice, €90.00 to Bob.
    ledger
        .commit(
            TransferBuilder::new()
                .deposit(alice, usd, money.parse("100.00")?, external)?
                .build(),
        )
        .await?;
    ledger
        .commit(
            TransferBuilder::new()
                .deposit(bob, eur, money.parse("90.00")?, external)?
                .build(),
        )
        .await?;

    println!("after funding:");
    print_balances(&ledger, alice, bob, usd, eur).await?;

    // Trade: Alice gives 100 USD to Bob; Bob gives 90 EUR to Alice. Both legs
    // settle in one atomic transfer — each asset is conserved independently.
    let trade = TransferBuilder::new()
        .movement(alice, bob, usd, money.parse("100.00")?)
        .movement(bob, alice, eur, money.parse("90.00")?)
        .build();
    ledger.commit(trade).await?;

    println!("after trade:");
    print_balances(&ledger, alice, bob, usd, eur).await?;

    Ok(())
}

async fn print_balances(
    ledger: &Arc<Ledger>,
    alice: AccountId,
    bob: AccountId,
    usd: AssetId,
    eur: AssetId,
) -> Result<(), Box<dyn std::error::Error>> {
    let money = Amount::new(2);
    println!(
        "  alice: {} USD, {} EUR",
        money.format(ledger.balance(&alice, &usd).await?),
        money.format(ledger.balance(&alice, &eur).await?),
    );
    println!(
        "  bob:   {} USD, {} EUR",
        money.format(ledger.balance(&bob, &usd).await?),
        money.format(ledger.balance(&bob, &eur).await?),
    );
    Ok(())
}

async fn connect() -> Result<Arc<Ledger>, Box<dyn std::error::Error>> {
    sqlx::any::install_default_drivers();
    let pool = sqlx::any::AnyPoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await?;
    let store = SqlStore::new(pool);
    store.migrate().await?;
    let ledger = Arc::new(Ledger::new(store));
    // On startup, finish any commit a crash interrupted (idempotent roll-forward).
    ledger.recover().await?;
    Ok(ledger)
}
