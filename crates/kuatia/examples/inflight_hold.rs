//! Authorize a payment hold, capture it in parts over time, then void the rest.
//!
//! This mirrors a card-style authorization: the customer's funds are parked in
//! per-destination holding accounts up front, captured (confirmed) in slices as
//! goods ship, and whatever is never captured is released (voided) back to the
//! customer. The `sleep` calls stand in for the real time that passes between
//! authorization, each capture, and the final release.
//!
//! Run with:
//! ```sh
//! cargo run -p kuatia --example inflight_hold
//! ```

use std::sync::Arc;
use std::time::Duration;

use kuatia::prelude::*;
use kuatia_storage_sql::SqlStore;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ledger = connect().await?;

    let customer = AccountId::new(1);
    let merchant = AccountId::new(2);
    let fee = AccountId::new(3);
    let external = AccountId::new(99);
    let usd = AssetId::new(1);
    let money = Amount::new(2); // two-decimal money

    ledger
        .create_account(Account::new(customer, AccountPolicy::NoOverdraft))
        .await?;
    ledger
        .create_account(Account::new(merchant, AccountPolicy::NoOverdraft))
        .await?;
    // The fee account collects the processing fee; a system account has no floor.
    ledger
        .create_account(Account::new(fee, AccountPolicy::SystemAccount))
        .await?;
    ledger
        .create_account(Account::new(external, AccountPolicy::ExternalAccount))
        .await?;

    // Fund the customer with $100.00.
    ledger
        .commit(
            TransferBuilder::new()
                .deposit(customer, usd, money.parse("100.00")?, external)?
                .build(),
        )
        .await?;
    println!("funded:");
    print_balances(&ledger, &money, customer, merchant, fee, usd).await?;

    // Authorize a $100 order: $90 destined for the merchant, $10 for the fee.
    // The funds leave the customer and park in one holding account per
    // destination. Nothing has reached the merchant or fee account yet.
    let order = TransferBuilder::new()
        .pay(customer, merchant, usd, money.parse("90.00")?)
        .pay(customer, fee, usd, money.parse("10.00")?)
        .build();
    let auth = ledger.authorize(order).await?;
    println!("\nauthorized (funds now held):");
    print_balances(&ledger, &money, customer, merchant, fee, usd).await?;
    print_status(&ledger, &money, &auth.inflight).await?;

    // Time passes before the first shipment.
    sleep(Duration::from_millis(300)).await;

    // First partial capture: ship $40 of goods, so capture $40 to the merchant.
    // The remaining $50 of the merchant hold stays parked.
    println!("\ncapture #1: $40.00 to the merchant");
    ledger
        .confirm(
            &auth.inflight,
            confirm_one(customer, merchant, usd, money.parse("40.00")?),
        )
        .await?;
    print_status(&ledger, &money, &auth.inflight).await?;

    // More time passes before the second shipment.
    sleep(Duration::from_millis(300)).await;

    // Second partial capture: ship another $20 to the merchant and take the full
    // $10 processing fee. The fee hold drains and closes; the merchant hold still
    // has $30 parked.
    println!("\ncapture #2: $20.00 to the merchant, $10.00 to the fee account");
    ledger
        .confirm(
            &auth.inflight,
            TransferBuilder::new()
                .pay(customer, merchant, usd, money.parse("20.00")?)
                .pay(customer, fee, usd, money.parse("10.00")?)
                .build(),
        )
        .await?;
    print_status(&ledger, &money, &auth.inflight).await?;

    // The order is finalized before everything was captured.
    sleep(Duration::from_millis(300)).await;

    // Void the remainder: the merchant hold's uncaptured $30 returns to the
    // customer. Already-captured amounts are untouched.
    println!("\nvoid: release the uncaptured remainder back to the customer");
    ledger.void(&auth.inflight).await?;
    print_status(&ledger, &money, &auth.inflight).await?;

    // Final tally: merchant captured $60, fee $10, customer got the $30 back.
    println!("\nfinal balances:");
    print_balances(&ledger, &money, customer, merchant, fee, usd).await?;

    Ok(())
}

/// A one-leg confirm set, built with the same `.pay()` shape as a transfer:
/// `from` is the leg's funder, `to` its destination.
fn confirm_one(from: AccountId, to: AccountId, asset: AssetId, amount: Cent) -> Transfer {
    TransferBuilder::new().pay(from, to, asset, amount).build()
}

async fn print_balances(
    ledger: &Arc<Ledger>,
    money: &Amount,
    customer: AccountId,
    merchant: AccountId,
    fee: AccountId,
    usd: AssetId,
) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "  customer: {}   merchant: {}   fee: {}",
        money.format(ledger.balance(&customer, &usd).await?),
        money.format(ledger.balance(&merchant, &usd).await?),
        money.format(ledger.balance(&fee, &usd).await?),
    );
    Ok(())
}

async fn print_status(
    ledger: &Arc<Ledger>,
    money: &Amount,
    inflight: &EnvelopeId,
) -> Result<(), Box<dyn std::error::Error>> {
    let status = ledger.inflight_status(inflight).await?;
    println!("  state: {:?}", status.state);
    for leg in &status.legs {
        println!(
            "    -> {:?}: authorized {}, confirmed {}, voided {}, held {}",
            leg.destination,
            money.format(leg.authorized),
            money.format(leg.confirmed),
            money.format(leg.voided),
            money.format(leg.held),
        );
    }
    Ok(())
}

/// Open a fresh in-memory SQLite database, run migrations, and wrap it in a
/// `Ledger`. Point the connection string at a file or a Postgres URL for a
/// persistent ledger.
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
