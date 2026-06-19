//! Fund an account, then withdraw value out of the ledger.
//!
//! Run with:
//! ```sh
//! cargo run -p kuatia --example withdraw
//! ```

use std::sync::Arc;

use kuatia::ledger::Ledger;
use kuatia_core::*;
use kuatia_storage_sql::SqlStore;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ledger = connect().await?;

    let alice = AccountId::new(1);
    let external = AccountId::new(99);
    let usd = AssetId::new(1);
    let money = Amount::new(2);

    ledger
        .create_account(Account::new(alice, AccountPolicy::NoOverdraft))
        .await?;
    ledger
        .create_account(Account::new(external, AccountPolicy::ExternalAccount))
        .await?;

    // Fund Alice with $100.00.
    ledger
        .commit(
            TransferBuilder::new()
                .deposit(alice, usd, money.parse("100.00")?, external)?
                .build(),
        )
        .await?;
    println!(
        "after deposit:  alice = {} USD",
        money.format(ledger.balance(&alice, &usd).await?)
    );

    // Withdraw $30.00 from Alice out to the external boundary account.
    ledger
        .commit(
            TransferBuilder::new()
                .withdraw(alice, usd, money.parse("30.00")?, external)
                .build(),
        )
        .await?;
    println!(
        "after withdraw: alice = {} USD",
        money.format(ledger.balance(&alice, &usd).await?)
    );

    // The external account carries the offset (negative) side: the mirror of the
    // value that currently sits inside the ledger.
    println!(
        "external boundary: {} USD",
        money.format(ledger.balance(&external, &usd).await?)
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
