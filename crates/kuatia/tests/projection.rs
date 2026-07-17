//! Balance-projection correctness (ADR-0019): the projection-aware read must
//! always equal the authoritative live-posting sum, and the projector must
//! advance the snapshot without changing the answer.

#![allow(missing_docs)]

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

async fn no_overdraft(ledger: &Arc<Ledger>, id: i64) {
    ledger
        .store()
        .create_account(Account::debit_must_not_exceed_credit(account(id)))
        .await
        .unwrap();
}

async fn overdraft(ledger: &Arc<Ledger>, id: i64) {
    ledger
        .store()
        .create_account(Account::new(account(id)))
        .await
        .unwrap();
}

async fn deposit(ledger: &Arc<Ledger>, to: i64, amount: i64) {
    let transfer = TransferBuilder::new()
        .deposit(account(to), usd(), Cent::from(amount), external())
        .unwrap()
        .build();
    ledger.commit(transfer).await.unwrap();
}

async fn pay(ledger: &Arc<Ledger>, from: i64, to: i64, amount: i64) {
    let transfer = TransferBuilder::new()
        .pay(account(from), account(to), usd(), Cent::from(amount))
        .build();
    ledger.commit(transfer).await.unwrap();
}

/// Assert the projection-aware read equals the authoritative live-posting sum
/// for every account, at every asset it might hold.
async fn assert_projection_matches(ledger: &Arc<Ledger>, ids: &[i64]) {
    for &id in ids {
        let authoritative = ledger.compute_balance(&account(id), &usd()).await.unwrap();
        let projected = ledger.balance(&account(id), &usd()).await.unwrap();
        assert_eq!(
            projected, authoritative,
            "projected balance for account {id} diverged from the live sum"
        );
    }
}

/// With no projector run at all (every projection absent, so the read folds the
/// whole history), the projection-aware read still equals the live sum through
/// deposits, change-making pays, and an overdraft offset posting.
#[tokio::test]
async fn projected_balance_matches_live_sum_without_refresh() {
    let ledger = Arc::new(Ledger::new(InMemoryStore::new()));
    no_overdraft(&ledger, 1).await;
    no_overdraft(&ledger, 2).await;
    overdraft(&ledger, 10).await;
    overdraft(&ledger, 99).await;

    deposit(&ledger, 1, 1000).await;
    // Several pays fragment account 1 into many change postings.
    pay(&ledger, 1, 2, 150).await;
    pay(&ledger, 1, 2, 70).await;
    pay(&ledger, 1, 2, 30).await;
    pay(&ledger, 2, 1, 40).await;
    // Overdraft: account 10 (empty balance) pays into a negative offset posting.
    pay(&ledger, 10, 2, 500).await;

    assert_projection_matches(&ledger, &[1, 2, 10, 99]).await;
}

/// Appending a cache point stores the balance directly, and the read still equals
/// the live sum. Appending only after all commits keeps this deterministic: grace
/// 0 puts the watermark at "now", so every committed transfer folds into the cache
/// point with an empty tail.
#[tokio::test]
async fn append_cache_point_snapshots_balance_and_preserves_answer() {
    let ledger = Arc::new(Ledger::new(InMemoryStore::new()).with_projection_grace_ms(0));
    no_overdraft(&ledger, 1).await;
    no_overdraft(&ledger, 2).await;
    overdraft(&ledger, 99).await;

    deposit(&ledger, 1, 1000).await;
    pay(&ledger, 1, 2, 250).await;
    pay(&ledger, 1, 2, 100).await;

    ledger
        .append_cache_point(&account(1), &usd())
        .await
        .unwrap();

    // The cache point holds account 1's balance (1000 - 250 - 100) directly.
    let cache_point = ledger
        .store()
        .get_closest_balance_projection(&account(1), &usd(), i64::MAX)
        .await
        .unwrap()
        .expect("a cache point exists after append");
    assert_eq!(cache_point.balance, Cent::from(650));
    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(650)
    );
    assert_projection_matches(&ledger, &[1, 2]).await;
}

/// Commit never writes a cache point: after commits with no read, none exists.
#[tokio::test]
async fn commit_does_not_write_cache_point() {
    let ledger = Arc::new(Ledger::new(InMemoryStore::new()));
    no_overdraft(&ledger, 1).await;
    no_overdraft(&ledger, 2).await;
    overdraft(&ledger, 99).await;

    deposit(&ledger, 1, 1000).await;
    pay(&ledger, 1, 2, 250).await;

    // No read has happened, so the lazy append never fired.
    assert!(
        ledger
            .store()
            .get_closest_balance_projection(&account(1), &usd(), i64::MAX)
            .await
            .unwrap()
            .is_none()
    );
}

/// A read appends a cache point once `snapshot_interval` credits/debits have
/// accrued (the append is spawned in the background; on the current-thread test
/// runtime it runs when we yield).
#[tokio::test]
async fn read_appends_cache_point_after_interval() {
    let ledger = Arc::new(
        Ledger::new(InMemoryStore::new())
            .with_projection_grace_ms(0)
            .with_snapshot_interval(1),
    );
    no_overdraft(&ledger, 1).await;
    no_overdraft(&ledger, 2).await;
    overdraft(&ledger, 99).await;

    deposit(&ledger, 1, 1000).await;
    pay(&ledger, 1, 2, 250).await;

    // This read folds >= 1 credit/debit for account 1, so it spawns an append.
    let _ = ledger.balance(&account(1), &usd()).await.unwrap();

    // Let the background append run, then confirm a cache point exists.
    let mut appeared = false;
    for _ in 0..1000 {
        tokio::task::yield_now().await;
        if ledger
            .store()
            .get_closest_balance_projection(&account(1), &usd(), i64::MAX)
            .await
            .unwrap()
            .is_some()
        {
            appeared = true;
            break;
        }
    }
    assert!(
        appeared,
        "a read past the interval should append a cache point"
    );
    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        ledger.compute_balance(&account(1), &usd()).await.unwrap()
    );
}

/// A read below the interval never appends a cache point: the background append
/// gates on new credits/debits, so a low-activity account accrues no rows (this
/// is what keeps a hot, frequently-read account from appending near-duplicates).
#[tokio::test]
async fn read_below_interval_appends_nothing() {
    let ledger = Arc::new(
        Ledger::new(InMemoryStore::new())
            .with_projection_grace_ms(0)
            .with_snapshot_interval(1_000),
    );
    no_overdraft(&ledger, 1).await;
    no_overdraft(&ledger, 2).await;
    overdraft(&ledger, 99).await;

    deposit(&ledger, 1, 1000).await;
    pay(&ledger, 1, 2, 250).await;

    // A handful of credits/debits, far below the 1000 interval.
    let bal = ledger.balance(&account(1), &usd()).await.unwrap();
    assert_eq!(
        bal,
        ledger.compute_balance(&account(1), &usd()).await.unwrap()
    );

    // Even after the background task has every chance to run, no cache point was
    // appended (the append gates on >= interval new credits/debits).
    for _ in 0..1000 {
        tokio::task::yield_now().await;
    }
    assert!(
        ledger
            .store()
            .get_closest_balance_projection(&account(1), &usd(), i64::MAX)
            .await
            .unwrap()
            .is_none(),
        "a below-interval read must not append a cache point"
    );
}

/// Deterministic xorshift64 PRNG so the property test is reproducible.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Property test: across a long random sequence of every UTXO-shaped operation
/// (deposits, change-making pays, overdraft offsets, multi-asset, subaccounts,
/// and reversals), the projection-aware read equals the authoritative live-posting
/// sum for every (account, asset) after every step. This is the empirical form of
/// the telescoping argument: whole-posting spends plus change-as-new-posting make
/// `snapshot + tail` fold exactly to the live set, in every shape the ledger can
/// produce.
#[tokio::test]
async fn projection_matches_live_sum_across_random_utxo_history() {
    // A low interval so reads append cache points throughout the run, exercising
    // the append-only cache points and closest-at-or-before selection. The
    // default grace keeps each watermark safely in the past (so the tail folds
    // every commit and no same-millisecond commit races an append); the invariant
    // holds regardless of cache-point state.
    let ledger = Arc::new(Ledger::new(InMemoryStore::new()).with_snapshot_interval(4));

    // Accounts under test, including two subaccounts and both overdraft kinds.
    let no_overdraft_ids = [
        AccountId::new(1),
        AccountId::new(2),
        AccountId::new(3),
        AccountId::with_sub(1, 7),
    ];
    let overdraft_ids = [
        AccountId::new(10),
        AccountId::new(11),
        AccountId::with_sub(11, 3),
    ];
    for id in no_overdraft_ids {
        ledger
            .store()
            .create_account(Account::debit_must_not_exceed_credit(id))
            .await
            .unwrap();
    }
    for id in overdraft_ids {
        ledger
            .store()
            .create_account(Account::new(id))
            .await
            .unwrap();
    }
    let ext = external();
    ledger
        .store()
        .create_account(Account::new(ext))
        .await
        .unwrap();

    let accounts: Vec<AccountId> = no_overdraft_ids
        .iter()
        .chain(overdraft_ids.iter())
        .copied()
        .collect();
    let assets = [AssetId::new(1), AssetId::new(2)];

    let mut rng = Rng(0x9e3779b97f4a7c15);
    let mut receipts: Vec<EnvelopeId> = Vec::new();

    for _ in 0..300 {
        let asset = assets[rng.below(assets.len() as u64) as usize];
        match rng.below(5) {
            // Deposit into a random account.
            0 => {
                let to = accounts[rng.below(accounts.len() as u64) as usize];
                let amount = 1 + rng.below(500) as i64;
                let t = TransferBuilder::new()
                    .deposit(to, asset, Cent::from(amount), ext)
                    .unwrap()
                    .build();
                if let Ok(r) = ledger.commit(t).await {
                    receipts.push(r.transfer_id);
                }
            }
            // Withdraw from a random account to the boundary.
            1 => {
                let from = accounts[rng.below(accounts.len() as u64) as usize];
                let amount = 1 + rng.below(200) as i64;
                let t = TransferBuilder::new()
                    .withdraw(from, asset, Cent::from(amount), ext)
                    .build();
                if let Ok(r) = ledger.commit(t).await {
                    receipts.push(r.transfer_id);
                }
            }
            // Reverse a previously committed transfer.
            2 if !receipts.is_empty() => {
                let id = receipts[rng.below(receipts.len() as u64) as usize];
                if let Ok(r) = ledger.reverse(&id).await {
                    receipts.push(r.transfer_id);
                }
            }
            // Pay between two random accounts (change / overdraft / cross-subaccount).
            _ => {
                let from = accounts[rng.below(accounts.len() as u64) as usize];
                let to = accounts[rng.below(accounts.len() as u64) as usize];
                if from == to {
                    continue;
                }
                let amount = 1 + rng.below(300) as i64;
                let t = TransferBuilder::new()
                    .pay(from, to, asset, Cent::from(amount))
                    .build();
                if let Ok(r) = ledger.commit(t).await {
                    receipts.push(r.transfer_id);
                }
            }
        }

        // Invariant: at rest after every step, the projection-aware read equals
        // the authoritative live-posting sum for every (account, asset).
        for account in accounts.iter().chain(std::iter::once(&ext)) {
            for asset in &assets {
                let authoritative = ledger.compute_balance(account, asset).await.unwrap();
                let projected = ledger.balance(account, asset).await.unwrap();
                assert_eq!(
                    projected, authoritative,
                    "projected != live sum for {account:?} / {asset:?}"
                );
            }
        }
    }
}

/// Concurrent commits from one funded account: after the dust settles, the
/// projection-aware read agrees with the live sum for every participant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn projection_matches_under_concurrent_commits() {
    let ledger = Arc::new(Ledger::new(InMemoryStore::new()));
    no_overdraft(&ledger, 1).await;
    for id in 2..=9 {
        no_overdraft(&ledger, id).await;
    }
    overdraft(&ledger, 99).await;
    deposit(&ledger, 1, 1000).await;

    let mut handles = Vec::new();
    for payee in 2..=9 {
        let ledger = Arc::clone(&ledger);
        handles.push(tokio::spawn(async move {
            let transfer = TransferBuilder::new()
                .pay(account(1), account(payee), usd(), Cent::from(10))
                .build();
            let _ = ledger.commit(transfer).await;
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    assert_projection_matches(&ledger, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 99]).await;
}
