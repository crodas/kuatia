#![allow(missing_docs)]

use std::sync::Arc;

use kuatia::error::LedgerError;
use kuatia::ledger::Ledger;
use kuatia::mem_store::InMemoryStore;
use kuatia::saga::*;
use kuatia_core::*;
use legend::{ExecutionResult, legend};
use std::collections::BTreeMap;

fn usd() -> AssetId {
    AssetId::new(1)
}

fn account(id: i64) -> AccountId {
    AccountId::new(id)
}

fn external() -> AccountId {
    AccountId::new(99)
}

fn make_account(id: i64, policy: AccountPolicy) -> Account {
    Account {
        id: AccountId::new(id),
        version: 1,
        policy,
        flags: AccountFlags::empty(),
        book: BookId(0),
        metadata: BTreeMap::new(),
    }
}

async fn setup_ledger() -> Arc<Ledger> {
    let store = InMemoryStore::new();
    let ledger = Arc::new(Ledger::new(store));

    for (id, policy) in [
        (1, AccountPolicy::NoOverdraft),
        (2, AccountPolicy::NoOverdraft),
        (3, AccountPolicy::NoOverdraft),
        (99, AccountPolicy::ExternalAccount),
    ] {
        ledger
            .store()
            .create_account(make_account(id, policy))
            .await
            .unwrap();
    }

    ledger
}

// Define a two-step saga: deposit then pay
legend! {
    FundAndPay<LedgerCtx, LedgerError> {
        deposit: DepositMovementStep,
        pay: PayMovementStep,
    }
}

#[tokio::test]
async fn saga_happy_path() {
    let ledger = setup_ledger().await;

    let saga = FundAndPay::new(FundAndPayInputs {
        deposit: DepositInput {
            to: account(1),
            asset: usd(),
            amount: Cent::from(100),
            external: external(),
        },
        pay: PayInput {
            from: account(1),
            to: account(2),
            asset: usd(),
            amount: Cent::from(60),
        },
    });

    let ctx = LedgerCtx::new(ledger.clone());
    let execution = saga.build(ctx);

    match execution.start().await {
        ExecutionResult::Completed(e) => {
            assert_eq!(e.context().receipts.len(), 2);
        }
        other => panic!("expected Completed, got {:?}", result_debug(&other)),
    }

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

// Define a saga that will fail on the second step and trigger compensation
legend! {
    DepositAndOverspend<LedgerCtx, LedgerError> {
        deposit: DepositMovementStep,
        pay: PayMovementStep,
    }
}

#[tokio::test]
async fn saga_compensation_on_failure() {
    let ledger = setup_ledger().await;

    // Deposit 50 then try to pay 100 -> pay fails -> deposit should be reversed
    let saga = DepositAndOverspend::new(DepositAndOverspendInputs {
        deposit: DepositInput {
            to: account(1),
            asset: usd(),
            amount: Cent::from(50),
            external: external(),
        },
        pay: PayInput {
            from: account(1),
            to: account(2),
            asset: usd(),
            amount: Cent::from(100), // more than available
        },
    });

    let ctx = LedgerCtx::new(ledger.clone());
    let execution = saga.build(ctx);

    match execution.start().await {
        ExecutionResult::Failed(_, err) => {
            // The saga carries the typed `LedgerError` across its step seam, so
            // the overspend surfaces as `Selection(InsufficientFunds)` rather
            // than a stringified `Store(Internal)`.
            assert!(
                matches!(
                    err,
                    LedgerError::Selection(SelectionError::InsufficientFunds { .. })
                ),
                "expected typed InsufficientFunds, got {err:?}"
            );
            // The deposit should have been compensated (reversed)
            // Note: balances won't be exactly 0 because the deposit reversal
            // creates new postings, but the net effect should be zero
            assert_eq!(
                ledger.balance(&account(1), &usd()).await.unwrap(),
                Cent::ZERO
            );
            assert_eq!(
                ledger.balance(&external(), &usd()).await.unwrap(),
                Cent::ZERO
            );
        }
        other => panic!("expected Failed, got {:?}", result_debug(&other)),
    }
}

// Three-step saga
legend! {
    ThreeStepFlow<LedgerCtx, LedgerError> {
        deposit: DepositMovementStep,
        pay_ab: PayMovementStep,
        pay_bc: PayMovementStep,
    }
}

#[tokio::test]
async fn saga_three_steps_happy() {
    let ledger = setup_ledger().await;

    let saga = ThreeStepFlow::new(ThreeStepFlowInputs {
        deposit: DepositInput {
            to: account(1),
            asset: usd(),
            amount: Cent::from(100),
            external: external(),
        },
        pay_ab: PayInput {
            from: account(1),
            to: account(2),
            asset: usd(),
            amount: Cent::from(60),
        },
        pay_bc: PayInput {
            from: account(2),
            to: account(3),
            asset: usd(),
            amount: Cent::from(30),
        },
    });

    let ctx = LedgerCtx::new(ledger.clone());
    let execution = saga.build(ctx);

    match execution.start().await {
        ExecutionResult::Completed(e) => {
            assert_eq!(e.context().receipts.len(), 3);
        }
        other => panic!("expected Completed, got {:?}", result_debug(&other)),
    }

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

fn result_debug<Ctx, Err, Steps>(r: &ExecutionResult<Ctx, Err, Steps>) -> &'static str
where
    Ctx: Send + Sync,
    Err: Send + Sync + Clone,
    Steps: legend::hlist::InstructionList<Ctx, Err>,
{
    match r {
        ExecutionResult::Completed(_) => "Completed",
        ExecutionResult::Paused(_) => "Paused",
        ExecutionResult::Failed(_, _) => "Failed",
        ExecutionResult::CompensationFailed { .. } => "CompensationFailed",
    }
}
