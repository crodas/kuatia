#![allow(missing_docs)]

use std::collections::BTreeMap;
use std::sync::Arc;

use kuatia::error::LedgerError;
use kuatia::ledger::Ledger;
use kuatia::mem_store::InMemoryStore;
use kuatia::saga::{DepositInput, Movement, PayInput, run_movements};
use kuatia_core::*;

fn usd() -> AssetId {
    AssetId::new(1)
}

fn account(id: i64) -> AccountId {
    AccountId::new(id)
}

fn external() -> AccountId {
    AccountId::new(99)
}

fn make_account(id: i64, flags: AccountFlags) -> Account {
    Account {
        id: AccountId::new(id),
        version: 1,
        flags,
        book: BookId(0),
        metadata: BTreeMap::new(),
    }
}

async fn setup_ledger() -> Arc<Ledger> {
    let store = InMemoryStore::new();
    let ledger = Arc::new(Ledger::new(store));

    for (id, policy) in [
        (1, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT),
        (2, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT),
        (3, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT),
        (99, AccountFlags::empty()),
    ] {
        ledger
            .store()
            .create_account(make_account(id, policy))
            .await
            .unwrap();
    }

    ledger
}

fn deposit(to: AccountId, amount: Cent) -> Movement {
    Movement::Deposit(DepositInput {
        to,
        asset: usd(),
        amount,
        external: external(),
    })
}

fn pay(from: AccountId, to: AccountId, amount: Cent) -> Movement {
    Movement::Pay(PayInput {
        from,
        to,
        asset: usd(),
        amount,
    })
}

/// A two-movement workflow (deposit then pay) commits both legs in order.
#[tokio::test]
async fn workflow_happy_path() {
    let ledger = setup_ledger().await;

    let receipts = run_movements(
        &ledger,
        &[
            deposit(account(1), Cent::from(100)),
            pay(account(1), account(2), Cent::from(60)),
        ],
    )
    .await
    .unwrap();
    assert_eq!(receipts.len(), 2);

    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(40)
    );
    assert_eq!(
        ledger.balance(&account(2), &usd()).await.unwrap(),
        Cent::from(60)
    );
    assert_eq!(
        ledger.balance(&external(), &usd()).await.unwrap(),
        Cent::from(-100)
    );
}

/// The second movement overspends and fails; the first (deposit) is reversed
/// LIFO, so the net effect is zero. The typed `InsufficientFunds` reaches the
/// caller.
#[tokio::test]
async fn workflow_compensation_on_failure() {
    let ledger = setup_ledger().await;

    // Deposit 50, then try to pay 100 (more than available) -> pay fails ->
    // deposit is reversed.
    let err = run_movements(
        &ledger,
        &[
            deposit(account(1), Cent::from(50)),
            pay(account(1), account(2), Cent::from(100)),
        ],
    )
    .await
    .unwrap_err();

    assert!(
        matches!(err, LedgerError::Selection(InsufficientFunds { .. })),
        "expected typed InsufficientFunds, got {err:?}"
    );
    // The deposit was compensated (reversed); the net effect is zero.
    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::ZERO
    );
    assert_eq!(
        ledger.balance(&external(), &usd()).await.unwrap(),
        Cent::ZERO
    );
}

/// A three-movement workflow commits all legs and settles the chain.
#[tokio::test]
async fn workflow_three_steps_happy() {
    let ledger = setup_ledger().await;

    let receipts = run_movements(
        &ledger,
        &[
            deposit(account(1), Cent::from(100)),
            pay(account(1), account(2), Cent::from(60)),
            pay(account(2), account(3), Cent::from(30)),
        ],
    )
    .await
    .unwrap();
    assert_eq!(receipts.len(), 3);

    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(40)
    );
    assert_eq!(
        ledger.balance(&account(2), &usd()).await.unwrap(),
        Cent::from(30)
    );
    assert_eq!(
        ledger.balance(&account(3), &usd()).await.unwrap(),
        Cent::from(30)
    );
}
