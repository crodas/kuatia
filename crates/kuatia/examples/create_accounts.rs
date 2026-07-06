//! Connect to a SQLite-backed ledger and create accounts.
//!
//! Run with:
//! ```sh
//! cargo run -p kuatia --example create_accounts
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;

use kuatia::ledger::Ledger;
use kuatia_core::*;
use kuatia_storage_sql::SqlStore;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ledger = connect().await?;

    // The common case is one line: a version-1 account with the given policy.
    ledger
        .create_account(Account::new(AccountId::new(1), AccountPolicy::NoOverdraft))
        .await?;
    ledger
        .create_account(Account::new(AccountId::new(2), AccountPolicy::NoOverdraft))
        .await?;
    // A system account (fees, settlement, market-making) — no balance floor.
    ledger
        .create_account(Account::new(
            AccountId::new(50),
            AccountPolicy::SystemAccount,
        ))
        .await?;

    // The same thing spelled out, so you can see every field of an `Account`.
    // This boundary account is where value enters/leaves the ledger.
    let external = Account {
        id: AccountId::new(99),
        version: 1,                             // accounts always start at version 1
        policy: AccountPolicy::ExternalAccount, // boundary for deposits/withdrawals
        flags: AccountFlags::empty(),           // not frozen, not closed
        book: DEFAULT_BOOK,                     // the implicit default book
        user_data: UserData::default(),         // fixed-width correlation slots
        metadata: BTreeMap::new(),              // free-form key/value metadata
    };
    ledger.create_account(external).await?;

    // Read them back (latest version of each).
    println!("accounts:");
    let mut accounts = ledger.list_accounts().await?;
    accounts.sort_by_key(|a| (a.id.id, a.id.sub));
    for a in &accounts {
        println!("  {:?}  policy={:?}  v{}", a.id, a.policy, a.version);
    }

    Ok(())
}

/// Open a fresh in-memory SQLite database, run migrations, and wrap it in a
/// `Ledger`. Point the connection string at a file (e.g.
/// `"sqlite://ledger.db?mode=rwc"`) or a Postgres URL for a persistent ledger.
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
    // A clean store has nothing pending, so this returns 0.
    ledger.recover().await?;
    Ok(ledger)
}
