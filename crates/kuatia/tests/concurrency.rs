//! Concurrency tests for the saga commit pipeline over `InMemoryStore`.
//!
//! `InMemoryStore` guards each field with a `tokio::RwLock`, so every individual
//! `Store` primitive is atomic. A saga, however, is a *sequence* of primitives
//! with no overarching lock, so the interesting races live between primitives
//! across concurrent sagas that share one `Arc<Ledger>`. The generated
//! conformance suite only drives the store sequentially, so none of this is
//! covered there.
//!
//! These tests run on a multi-thread runtime and use `tokio::spawn` so the
//! sagas genuinely interleave rather than run to completion one at a time.

#![allow(missing_docs)]

use std::collections::BTreeMap;
use std::sync::Arc;

use kuatia::ledger::Ledger;
use kuatia::mem_store::InMemoryStore;
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

fn make_account(id: i64, policy: AccountPolicy) -> Account {
    Account {
        id: AccountId::new(id),
        version: 1,
        policy,
        flags: AccountFlags::empty(),
        book: BookId(0),
        user_data: UserData::default(),
        metadata: BTreeMap::new(),
    }
}

/// A ledger with `NoOverdraft` accounts `1..=n` plus an external account.
async fn ledger_with_accounts(n: i64) -> Arc<Ledger> {
    let ledger = Arc::new(Ledger::new(InMemoryStore::new()));
    for id in 1..=n {
        ledger
            .store()
            .create_account(make_account(id, AccountPolicy::NoOverdraft))
            .await
            .unwrap();
    }
    ledger
        .store()
        .create_account(make_account(99, AccountPolicy::ExternalAccount))
        .await
        .unwrap();
    ledger
}

async fn deposit(ledger: &Arc<Ledger>, to: AccountId, amount: Cent) {
    let transfer = TransferBuilder::new()
        .deposit(to, usd(), amount, external())
        .unwrap()
        .build();
    ledger.commit(transfer).await.unwrap();
}

// ---------------------------------------------------------------------------
// 1. Double-spend prevention (the headline invariant)
// ---------------------------------------------------------------------------

/// Many transfers concurrently try to spend the *same* funded posting to
/// different recipients. Exactly one may win: the winner's `reserve_postings`
/// flips the single Active posting to `PendingInactive`, and every other saga's
/// reserve returns zero for a fresh reservation, so it fails and compensates.
/// The ledger stays conserved: the payer ends at zero and exactly one recipient
/// receives the full amount.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_double_spend_has_one_winner() {
    const RECIPIENTS: i64 = 8;
    let ledger = ledger_with_accounts(1 + RECIPIENTS).await;

    // Account 1 holds a single Active posting of 100.
    deposit(&ledger, account(1), Cent::from(100)).await;

    // Fire one full-balance payment per recipient, all at once.
    let mut handles = Vec::new();
    for recipient in 2..=(1 + RECIPIENTS) {
        let ledger = Arc::clone(&ledger);
        handles.push(tokio::spawn(async move {
            let transfer = TransferBuilder::new()
                .pay(account(1), account(recipient), usd(), Cent::from(100))
                .build();
            ledger.commit(transfer).await
        }));
    }

    let mut winners = 0;
    for h in handles {
        if h.await.unwrap().is_ok() {
            winners += 1;
        }
    }
    assert_eq!(winners, 1, "exactly one concurrent spend may succeed");

    // Conservation: payer drained, exactly one recipient credited, total = 100.
    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::ZERO
    );
    let mut credited = 0;
    let mut total = Cent::ZERO;
    for recipient in 2..=(1 + RECIPIENTS) {
        let bal = ledger.balance(&account(recipient), &usd()).await.unwrap();
        if bal != Cent::ZERO {
            credited += 1;
            assert_eq!(bal, Cent::from(100));
        }
        total = total.checked_add(bal).unwrap();
    }
    assert_eq!(credited, 1, "exactly one recipient is credited");
    assert_eq!(total, Cent::from(100), "value is conserved");
}

// ---------------------------------------------------------------------------
// 2. Idempotency
// ---------------------------------------------------------------------------

/// Re-committing an already-committed envelope returns the same receipt and does
/// not move value a second time. This is the sequential idempotency contract
/// that `commit_envelope` guarantees via its content-addressed short-circuit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recommit_same_envelope_is_idempotent() {
    let ledger = ledger_with_accounts(2).await;
    deposit(&ledger, account(1), Cent::from(100)).await;

    let transfer = TransferBuilder::new()
        .pay(account(1), account(2), usd(), Cent::from(50))
        .build();
    let envelope = ledger.resolve(&transfer).await.unwrap();

    let first = ledger.commit_envelope(envelope.clone()).await.unwrap();
    let second = ledger.commit_envelope(envelope).await.unwrap();

    assert_eq!(first, second, "replay returns the original receipt");
    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(50)
    );
    assert_eq!(
        ledger.balance(&account(2), &usd()).await.unwrap(),
        Cent::from(50)
    );
}

/// The same envelope committed concurrently from many tasks. Because the
/// content-addressed id is the idempotency key, value moves exactly once no
/// matter how the sagas interleave: some tasks win or observe the stored
/// transfer and return its receipt; the rest lose the reservation race and
/// fail. Every successful receipt is identical, and the balances move once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_identical_commits_move_value_once() {
    const TASKS: usize = 8;
    let ledger = ledger_with_accounts(2).await;
    deposit(&ledger, account(1), Cent::from(100)).await;

    let transfer = TransferBuilder::new()
        .pay(account(1), account(2), usd(), Cent::from(50))
        .build();
    let envelope = ledger.resolve(&transfer).await.unwrap();

    let mut handles = Vec::new();
    for _ in 0..TASKS {
        let ledger = Arc::clone(&ledger);
        let envelope = envelope.clone();
        handles.push(tokio::spawn(async move {
            ledger.commit_envelope(envelope).await
        }));
    }

    let mut receipts = Vec::new();
    for h in handles {
        if let Ok(receipt) = h.await.unwrap() {
            receipts.push(receipt);
        }
    }

    assert!(!receipts.is_empty(), "at least one commit succeeds");
    let first = &receipts[0];
    assert!(
        receipts.iter().all(|r| r == first),
        "every successful commit returns the same receipt"
    );

    // Value moved exactly once, and exactly one transfer is stored.
    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(50)
    );
    assert_eq!(
        ledger.balance(&account(2), &usd()).await.unwrap(),
        Cent::from(50)
    );
    assert!(
        ledger
            .store()
            .get_transfer(&first.transfer_id)
            .await
            .unwrap()
            .is_some(),
        "the committed transfer is persisted"
    );
}

// ---------------------------------------------------------------------------
// 3. Freeze vs. commit race
// ---------------------------------------------------------------------------

/// Freezing an account concurrently with a payment out of it must leave a
/// consistent state. The account is versioned and the commit pins the snapshot
/// it validated against, so the two serialize one way or the other: either the
/// payment finalizes first (against the unfrozen snapshot) and the freeze lands
/// on top, or the freeze bumps the version first and the commit's last-step
/// re-validation rejects the now-frozen account. There is no middle ground where
/// value moves out of a frozen account against a stale snapshot. Value is always
/// conserved and the payment is all-or-nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn freeze_during_commit_stays_consistent() {
    // Race is timing-dependent; run several fresh rounds to sample interleavings.
    for _ in 0..24 {
        let ledger = ledger_with_accounts(2).await;
        deposit(&ledger, account(1), Cent::from(100)).await;

        let freezer = {
            let ledger = Arc::clone(&ledger);
            tokio::spawn(async move { ledger.freeze(&account(1)).await })
        };
        let payer = {
            let ledger = Arc::clone(&ledger);
            tokio::spawn(async move {
                let transfer = TransferBuilder::new()
                    .pay(account(1), account(2), usd(), Cent::from(50))
                    .build();
                ledger.commit(transfer).await
            })
        };
        freezer.await.unwrap().expect("freeze always succeeds");
        let paid = payer.await.unwrap().is_ok();

        let b1 = ledger.balance(&account(1), &usd()).await.unwrap();
        let b2 = ledger.balance(&account(2), &usd()).await.unwrap();

        // Conservation and all-or-nothing, keyed on whether the pay committed.
        assert_eq!(
            b1.checked_add(b2).unwrap(),
            Cent::from(100),
            "value is conserved regardless of who won"
        );
        if paid {
            assert_eq!(b1, Cent::from(50));
            assert_eq!(b2, Cent::from(50));
        } else {
            assert_eq!(b1, Cent::from(100));
            assert_eq!(b2, Cent::ZERO);
        }

        // The account is frozen either way; no further payment may leave it.
        assert!(ledger.get_account(&account(1)).await.unwrap().is_frozen());
        let after = TransferBuilder::new()
            .pay(account(1), account(2), usd(), Cent::from(10))
            .build();
        assert!(
            ledger.commit(after).await.is_err(),
            "a frozen account cannot pay"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Disjoint transfers all commit and conserve
// ---------------------------------------------------------------------------

/// Concurrent transfers over non-overlapping accounts never contend, so all of
/// them commit and total value is conserved. This is the throughput counterpart
/// to the double-spend test: parallelism is only constrained where postings are
/// actually shared.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disjoint_transfers_all_commit_and_conserve() {
    const PAIRS: i64 = 8;
    // Accounts 1..=2*PAIRS: odd = payer (funded), even = payee.
    let ledger = ledger_with_accounts(2 * PAIRS).await;
    for k in 0..PAIRS {
        deposit(&ledger, account(2 * k + 1), Cent::from(100)).await;
    }

    let mut handles = Vec::new();
    for k in 0..PAIRS {
        let ledger = Arc::clone(&ledger);
        handles.push(tokio::spawn(async move {
            let transfer = TransferBuilder::new()
                .pay(
                    account(2 * k + 1),
                    account(2 * k + 2),
                    usd(),
                    Cent::from(100),
                )
                .build();
            ledger.commit(transfer).await
        }));
    }
    for h in handles {
        h.await.unwrap().expect("disjoint transfers never contend");
    }

    let mut total = Cent::ZERO;
    for id in 1..=(2 * PAIRS) {
        let bal = ledger.balance(&account(id), &usd()).await.unwrap();
        let expected = if id % 2 == 0 {
            Cent::from(100)
        } else {
            Cent::ZERO
        };
        assert_eq!(bal, expected, "account {id} settled");
        total = total.checked_add(bal).unwrap();
    }
    assert_eq!(total, Cent::from(100 * PAIRS), "value is conserved");
}

// ---------------------------------------------------------------------------
// 5. Overdraft floor is best-effort under concurrency (documented limitation)
// ---------------------------------------------------------------------------

/// Documents a known, accepted limitation: the `CappedOverdraft` floor is
/// re-checked at the last step before writing, but that check is not atomic
/// with the write. Two overdrafts that each pass the floor check against the
/// same pre-transfer balance can both commit and jointly push the account below
/// its floor. See `doc/transfers.md`.
///
/// This test is `#[ignore]`d because the breach is timing-dependent, so it is
/// executable documentation rather than a CI assertion. What always holds, and
/// what it does assert, is per-asset conservation: the overdraft's negative
/// postings are real value owed, never minted. If a run drives the account below
/// the floor, that is the documented behavior, not a conservation failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "documents the best-effort overdraft floor; breach is timing-dependent"]
async fn overdraft_floor_is_best_effort_under_concurrency() {
    let floor = Cent::from(-100);
    let mut observed_breach = false;

    const PAYEES: i64 = 8;
    for _ in 0..64 {
        let ledger = Arc::new(Ledger::new(InMemoryStore::new()));
        ledger
            .store()
            .create_account(make_account(1, AccountPolicy::CappedOverdraft { floor }))
            .await
            .unwrap();
        for payee in 2..=(1 + PAYEES) {
            ledger
                .store()
                .create_account(make_account(payee, AccountPolicy::NoOverdraft))
                .await
                .unwrap();
        }

        // One payment of 60 to each distinct payee from an empty overdraft
        // account (distinct payees keep the envelopes distinct, so they are not
        // collapsed by content-addressed idempotency). Each alone projects to
        // -60 (within the -100 floor); any two that slip through the last-step
        // floor check together already breach it.
        let mut handles = Vec::new();
        for payee in 2..=(1 + PAYEES) {
            let ledger = Arc::clone(&ledger);
            handles.push(tokio::spawn(async move {
                let transfer = TransferBuilder::new()
                    .pay(account(1), account(payee), usd(), Cent::from(60))
                    .build();
                ledger.commit(transfer).await
            }));
        }
        for h in handles {
            let _ = h.await.unwrap();
        }

        let mut total = ledger.balance(&account(1), &usd()).await.unwrap();
        for payee in 2..=(1 + PAYEES) {
            total = total
                .checked_add(ledger.balance(&account(payee), &usd()).await.unwrap())
                .unwrap();
        }
        assert_eq!(
            total,
            Cent::ZERO,
            "value is conserved even when the floor is breached"
        );
        if ledger.balance(&account(1), &usd()).await.unwrap() < floor {
            observed_breach = true;
        }
    }

    eprintln!(
        "overdraft floor breach observed under concurrency: {observed_breach} \
         (best-effort by design; see doc/transfers.md)"
    );
}
