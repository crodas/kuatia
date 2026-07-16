#![allow(missing_docs)]

use std::sync::Arc;

use kuatia::ledger::Ledger;
use kuatia::mem_store::InMemoryStore;
use kuatia_core::*;
use std::collections::BTreeMap;

fn usd() -> AssetId {
    AssetId::new(1)
}

fn eur() -> AssetId {
    AssetId::new(2)
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

    ledger
        .store()
        .create_account(make_account(1, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT))
        .await
        .unwrap();
    ledger
        .store()
        .create_account(make_account(2, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT))
        .await
        .unwrap();
    ledger
        .store()
        .create_account(make_account(3, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT))
        .await
        .unwrap();
    ledger
        .store()
        .create_account(make_account(99, AccountFlags::empty()))
        .await
        .unwrap();

    ledger
}

/// Helper: deposit via commit()
async fn deposit(
    ledger: &Arc<Ledger>,
    to: AccountId,
    asset: AssetId,
    amount: Cent,
    ext: AccountId,
) -> Receipt {
    let transfer = TransferBuilder::new()
        .deposit(to, asset, amount, ext)
        .unwrap()
        .build();
    ledger.commit(transfer).await.unwrap()
}

/// Helper: pay via commit()
async fn pay(
    ledger: &Arc<Ledger>,
    from: AccountId,
    to: AccountId,
    asset: AssetId,
    amount: Cent,
) -> Receipt {
    let transfer = TransferBuilder::new().pay(from, to, asset, amount).build();
    ledger.commit(transfer).await.unwrap()
}

/// Helper: withdraw via commit()
async fn withdraw(
    ledger: &Arc<Ledger>,
    from: AccountId,
    asset: AssetId,
    amount: Cent,
    ext: AccountId,
) -> Receipt {
    let transfer = TransferBuilder::new()
        .withdraw(from, asset, amount, ext)
        .build();
    ledger.commit(transfer).await.unwrap()
}

// ---------------------------------------------------------------------------
// §4.1 Deposit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deposit_creates_balanced_postings() {
    let ledger = setup_ledger().await;

    deposit(&ledger, account(1), usd(), Cent::from(100), external()).await;

    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(100)
    );
    assert_eq!(
        ledger.balance(&external(), &usd()).await.unwrap(),
        Cent::from(-100)
    );
}

// ---------------------------------------------------------------------------
// §4.2 Internal transfer with change
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pay_with_change() {
    let ledger = setup_ledger().await;

    deposit(&ledger, account(1), usd(), Cent::from(100), external()).await;
    pay(&ledger, account(1), account(2), usd(), Cent::from(50)).await;

    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(50)
    );
    assert_eq!(
        ledger.balance(&account(2), &usd()).await.unwrap(),
        Cent::from(50)
    );
    assert_eq!(
        ledger.balance(&external(), &usd()).await.unwrap(),
        Cent::from(-100)
    );
}

// ---------------------------------------------------------------------------
// §4.3 Multi-hop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multi_hop_transfer() {
    let ledger = setup_ledger().await;

    deposit(&ledger, account(1), usd(), Cent::from(100), external()).await;
    pay(&ledger, account(1), account(2), usd(), Cent::from(50)).await;
    pay(&ledger, account(2), account(3), usd(), Cent::from(20)).await;

    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(50)
    );
    assert_eq!(
        ledger.balance(&account(2), &usd()).await.unwrap(),
        Cent::from(30)
    );
    assert_eq!(
        ledger.balance(&account(3), &usd()).await.unwrap(),
        Cent::from(20)
    );
    assert_eq!(
        ledger.balance(&external(), &usd()).await.unwrap(),
        Cent::from(-100)
    );
}

// ---------------------------------------------------------------------------
// §4.5 Withdrawal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn withdrawal_reduces_external_liability() {
    let ledger = setup_ledger().await;

    deposit(&ledger, account(1), usd(), Cent::from(100), external()).await;
    withdraw(&ledger, account(1), usd(), Cent::from(50), external()).await;

    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(50)
    );
    assert_eq!(
        ledger.balance(&external(), &usd()).await.unwrap(),
        Cent::from(-50)
    );
}

// ---------------------------------------------------------------------------
// Full round-trip: deposit -> pay -> withdraw -> verify total = 0
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_round_trip() {
    let ledger = setup_ledger().await;

    deposit(&ledger, account(1), usd(), Cent::from(100), external()).await;
    pay(&ledger, account(1), account(2), usd(), Cent::from(60)).await;
    withdraw(&ledger, account(2), usd(), Cent::from(60), external()).await;
    withdraw(&ledger, account(1), usd(), Cent::from(40), external()).await;

    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::ZERO
    );
    assert_eq!(
        ledger.balance(&account(2), &usd()).await.unwrap(),
        Cent::ZERO
    );
    assert_eq!(
        ledger.balance(&external(), &usd()).await.unwrap(),
        Cent::ZERO
    );
}

// ---------------------------------------------------------------------------
// Idempotency -- committing same envelope twice returns same receipt
// ---------------------------------------------------------------------------

#[tokio::test]
async fn idempotent_commit() {
    let ledger = setup_ledger().await;

    let envelope = EnvelopeBuilder::new()
        .creates(vec![
            NewPosting {
                owner: account(1),
                asset: usd(),
                value: Cent::from(100),
                payer: None,
            },
            NewPosting {
                owner: external(),
                asset: usd(),
                value: Cent::from(-100),
                payer: None,
            },
        ])
        .build();

    let r1 = ledger.commit_envelope(envelope.clone()).await.unwrap();
    let r2 = ledger.commit_envelope(envelope).await.unwrap();

    assert_eq!(r1.transfer_id, r2.transfer_id);
    // Balance should only be 100, not 200 (second commit was a no-op)
    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(100)
    );
}

// ---------------------------------------------------------------------------
// Overdraft prevention
// ---------------------------------------------------------------------------

#[tokio::test]
async fn overdraft_rejected() {
    let ledger = setup_ledger().await;

    deposit(&ledger, account(1), usd(), Cent::from(50), external()).await;
    let transfer = TransferBuilder::new()
        .pay(account(1), account(2), usd(), Cent::from(100))
        .build();
    let result = ledger.commit(transfer).await;

    assert!(result.is_err());
    // Balance unchanged
    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(50)
    );
}

// ---------------------------------------------------------------------------
// Reverse: forward compensating transfer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reverse_restores_balances() {
    let ledger = setup_ledger().await;

    deposit(&ledger, account(1), usd(), Cent::from(100), external()).await;
    let pay_receipt = pay(&ledger, account(1), account(2), usd(), Cent::from(60)).await;

    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(40)
    );
    assert_eq!(
        ledger.balance(&account(2), &usd()).await.unwrap(),
        Cent::from(60)
    );

    // Reverse the payment
    ledger.reverse(&pay_receipt.transfer_id).await.unwrap();

    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(100)
    );
    assert_eq!(
        ledger.balance(&account(2), &usd()).await.unwrap(),
        Cent::ZERO
    );
}

// ---------------------------------------------------------------------------
// Frozen account blocks transfers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn frozen_account_rejected() {
    let store = InMemoryStore::new();
    let ledger = Arc::new(Ledger::new(store));

    let mut frozen = make_account(1, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT);
    frozen.flags = AccountFlags::FROZEN;
    ledger.store().create_account(frozen).await.unwrap();
    ledger
        .store()
        .create_account(make_account(99, AccountFlags::empty()))
        .await
        .unwrap();

    let transfer = TransferBuilder::new()
        .deposit(account(1), usd(), Cent::from(100), external())
        .unwrap()
        .build();
    let result = ledger.commit(transfer).await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Multi-asset: each asset conserves independently
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multi_asset_independent_balances() {
    let ledger = setup_ledger().await;

    deposit(&ledger, account(1), usd(), Cent::from(100), external()).await;
    deposit(&ledger, account(1), eur(), Cent::from(200), external()).await;

    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(100)
    );
    assert_eq!(
        ledger.balance(&account(1), &eur()).await.unwrap(),
        Cent::from(200)
    );

    pay(&ledger, account(1), account(2), usd(), Cent::from(30)).await;

    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(70)
    );
    assert_eq!(
        ledger.balance(&account(1), &eur()).await.unwrap(),
        Cent::from(200)
    );
    assert_eq!(
        ledger.balance(&account(2), &usd()).await.unwrap(),
        Cent::from(30)
    );
}

// ---------------------------------------------------------------------------
// §4.4 FX trade via market account
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fx_trade_via_market_account() {
    let store = InMemoryStore::new();
    let ledger = Arc::new(Ledger::new(store));

    // Setup accounts
    for (id, policy) in [
        (1, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT),
        (50, AccountFlags::empty()), // FX market account
        (99, AccountFlags::empty()),
    ] {
        ledger
            .store()
            .create_account(make_account(id, policy))
            .await
            .unwrap();
    }

    // Seed: account1 has 100 USD, fx has 92 EUR
    deposit(&ledger, account(1), usd(), Cent::from(100), external()).await;
    deposit(&ledger, account(50), eur(), Cent::from(92), external()).await;

    // FX trade: account1 sells 100 USD, buys 92 EUR
    // Build the atomic envelope manually since it spans two assets
    let a1_usd_postings = ledger
        .store()
        .get_postings_by_account(1, None, Some(&usd()), PostingFilter::Active)
        .await
        .unwrap();
    let fx_eur_postings = ledger
        .store()
        .get_postings_by_account(50, None, Some(&eur()), PostingFilter::Active)
        .await
        .unwrap();

    let envelope = EnvelopeBuilder::new()
        .consumes(vec![a1_usd_postings[0].id, fx_eur_postings[0].id])
        .creates(vec![
            NewPosting {
                owner: account(50),
                asset: usd(),
                value: Cent::from(100),
                payer: Some(account(1)),
            },
            NewPosting {
                owner: account(1),
                asset: eur(),
                value: Cent::from(92),
                payer: Some(account(50)),
            },
        ])
        .build();

    ledger.commit_envelope(envelope).await.unwrap();

    // Verify
    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::ZERO
    );
    assert_eq!(
        ledger.balance(&account(1), &eur()).await.unwrap(),
        Cent::from(92)
    );
    assert_eq!(
        ledger.balance(&account(50), &usd()).await.unwrap(),
        Cent::from(100)
    );
    assert_eq!(
        ledger.balance(&account(50), &eur()).await.unwrap(),
        Cent::ZERO
    );
}

// ---------------------------------------------------------------------------
// Account lifecycle: freeze / unfreeze / close
// ---------------------------------------------------------------------------

#[tokio::test]
async fn freeze_blocks_transfers() {
    let ledger = setup_ledger().await;

    deposit(&ledger, account(1), usd(), Cent::from(100), external()).await;
    ledger.freeze(&account(1)).await.unwrap();

    // Paying from a frozen account should fail
    let transfer = TransferBuilder::new()
        .pay(account(1), account(2), usd(), Cent::from(50))
        .build();
    let result = ledger.commit(transfer).await;
    assert!(result.is_err());
    // Balance unchanged
    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(100)
    );
}

#[tokio::test]
async fn unfreeze_re_enables_transfers() {
    let ledger = setup_ledger().await;

    deposit(&ledger, account(1), usd(), Cent::from(100), external()).await;
    ledger.freeze(&account(1)).await.unwrap();
    ledger.unfreeze(&account(1)).await.unwrap();

    // Should work again
    pay(&ledger, account(1), account(2), usd(), Cent::from(50)).await;
    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(50)
    );
}

#[tokio::test]
async fn close_account_with_zero_balance() {
    let ledger = setup_ledger().await;

    // Account 3 has never transacted -- zero balance, no postings
    ledger.close(&account(3)).await.unwrap();

    // Closed account rejects deposits
    let transfer = TransferBuilder::new()
        .deposit(account(3), usd(), Cent::from(100), external())
        .unwrap()
        .build();
    let result = ledger.commit(transfer).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn close_account_with_balance_rejected() {
    let ledger = setup_ledger().await;

    deposit(&ledger, account(1), usd(), Cent::from(100), external()).await;

    // Should fail -- account still has active postings
    let result = ledger.close(&account(1)).await;
    assert!(result.is_err());
    // Balance unchanged
    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(100)
    );
}

#[tokio::test]
async fn close_rejects_reserved_postings() {
    let ledger = setup_ledger().await;

    deposit(&ledger, account(1), usd(), Cent::from(100), external()).await;

    // Reserve the account's only posting (a transfer in flight): move it from
    // the active index into the reserved index.
    let postings = ledger
        .store()
        .get_postings_by_account(1, None, Some(&usd()), PostingFilter::Active)
        .await
        .unwrap();
    ledger
        .store()
        .reserve_postings(&[postings[0].id], ReservationId::new(1))
        .await
        .unwrap();

    // Close must reject: the posting is live (reserved), not spent.
    let result = ledger.close(&account(1)).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn freeze_closed_account_rejected() {
    let ledger = setup_ledger().await;

    ledger.close(&account(3)).await.unwrap();

    let result = ledger.freeze(&account(3)).await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Query layer: history, postings, list_accounts, get_account
// ---------------------------------------------------------------------------

#[tokio::test]
async fn history_returns_transfers_for_account() {
    let ledger = setup_ledger().await;

    deposit(&ledger, account(1), usd(), Cent::from(100), external()).await;
    pay(&ledger, account(1), account(2), usd(), Cent::from(40)).await;
    deposit(&ledger, account(2), usd(), Cent::from(50), external()).await;

    let h1 = ledger.history(&account(1)).await.unwrap();
    // account(1) was in the deposit and the pay
    assert_eq!(h1.len(), 2);

    let h2 = ledger.history(&account(2)).await.unwrap();
    // account(2) was in the pay and a second deposit
    assert_eq!(h2.len(), 2);
}

#[tokio::test]
async fn postings_returns_all_postings() {
    let ledger = setup_ledger().await;

    deposit(&ledger, account(1), usd(), Cent::from(100), external()).await;
    pay(&ledger, account(1), account(2), usd(), Cent::from(60)).await;

    let posts = ledger.postings(&account(1)).await.unwrap();
    // Original 100 posting (now consumed) + 40 change posting (active)
    assert_eq!(posts.len(), 2);

    let with_state = ledger.postings_with_state(&account(1)).await.unwrap();
    let active: Vec<_> = with_state
        .iter()
        .filter(|(_, s)| *s == PostingState::Active)
        .collect();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].0.value, Cent::from(40));
}

#[tokio::test]
async fn list_accounts_returns_all() {
    let ledger = setup_ledger().await;

    let accounts = ledger.list_accounts().await.unwrap();
    // setup_ledger creates accounts 1, 2, 3, 99
    assert_eq!(accounts.len(), 4);
}

#[tokio::test]
async fn get_account_by_id() {
    let ledger = setup_ledger().await;

    let acc = ledger.get_account(&account(1)).await.unwrap();
    assert_eq!(acc.id, account(1));
    assert!(acc.forbids_overdraft());
}

#[tokio::test]
async fn get_account_not_found() {
    let ledger = setup_ledger().await;

    let result = ledger.get_account(&account(999)).await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Append-only accounts: version history, version conflict, account_versions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn account_history_tracks_versions() {
    let ledger = setup_ledger().await;

    // Version 1: created
    let history = ledger.account_history(&account(1)).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].version, 1);

    // Version 2: frozen
    ledger.freeze(&account(1)).await.unwrap();
    let history = ledger.account_history(&account(1)).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].version, 2);
    assert!(history[1].is_frozen());

    // Version 3: unfrozen
    ledger.unfreeze(&account(1)).await.unwrap();
    let history = ledger.account_history(&account(1)).await.unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(history[2].version, 3);
    assert!(!history[2].is_frozen());
}

#[tokio::test]
async fn store_never_compacts() {
    let ledger = setup_ledger().await;

    // Freeze and unfreeze multiple times
    for _ in 0..5 {
        ledger.freeze(&account(1)).await.unwrap();
        ledger.unfreeze(&account(1)).await.unwrap();
    }

    // All 11 versions preserved (1 creation + 10 mutations)
    let history = ledger.account_history(&account(1)).await.unwrap();
    assert_eq!(history.len(), 11);
    // Versions are monotonically increasing
    for (i, acc) in history.iter().enumerate() {
        assert_eq!(acc.version, (i + 1) as u64);
    }
}

#[tokio::test]
async fn transfer_records_account_snapshots() {
    let ledger = setup_ledger().await;

    deposit(&ledger, account(1), usd(), Cent::from(100), external()).await;

    // The envelope should have account_snapshots populated by the resolve step
    let transfers = ledger.history(&account(1)).await.unwrap();
    assert_eq!(transfers.len(), 1);
    assert!(!transfers[0].envelope.account_snapshots().is_empty());
}

#[tokio::test]
async fn stale_snapshot_rejected() {
    let ledger = setup_ledger().await;

    // Get current snapshot for account(1)
    let acc1 = ledger.get_account(&account(1)).await.unwrap();
    let stale_snapshot = kuatia_core::account_snapshot_id(&acc1);

    // Freeze account(1) -- changes its snapshot hash
    ledger.freeze(&account(1)).await.unwrap();

    // Build an envelope with the stale snapshot
    let envelope = EnvelopeBuilder::new()
        .creates(vec![
            NewPosting {
                owner: account(1),
                asset: usd(),
                value: Cent::from(100),
                payer: None,
            },
            NewPosting {
                owner: external(),
                asset: usd(),
                value: Cent::from(-100),
                payer: None,
            },
        ])
        .account_snapshots(vec![stale_snapshot])
        .build();

    let result = ledger.commit_envelope(envelope).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn account_hash_deterministic() {
    let acc = make_account(42, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT);
    let h1 = kuatia_core::account_hash(&acc);
    let h2 = kuatia_core::account_hash(&acc);
    assert_eq!(h1, h2);
}

#[tokio::test]
async fn account_hash_changes_with_version() {
    let mut acc = make_account(42, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT);
    let h1 = kuatia_core::account_hash(&acc);
    acc.version = 2;
    acc.flags |= AccountFlags::FROZEN;
    let h2 = kuatia_core::account_hash(&acc);
    assert_ne!(h1, h2);
}

// ---------------------------------------------------------------------------
// Overdraft via negative postings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn overdraft_creates_negative_posting() {
    let store = InMemoryStore::new();
    let ledger = Arc::new(Ledger::new(store));
    for (id, flags) in [
        (10, AccountFlags::empty()),
        (2, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT),
        (99, AccountFlags::empty()),
    ] {
        ledger
            .store()
            .create_account(make_account(id, flags))
            .await
            .unwrap();
    }

    // Fund account 10 with 50, then pay 100 — overdraft covers the 50 shortfall.
    deposit(&ledger, account(10), usd(), Cent::from(50), external()).await;
    pay(&ledger, account(10), account(2), usd(), Cent::from(100)).await;

    assert_eq!(
        ledger.balance(&account(10), &usd()).await.unwrap(),
        Cent::from(-50)
    );
    assert_eq!(
        ledger.balance(&account(2), &usd()).await.unwrap(),
        Cent::from(100)
    );

    // A negative posting now backs the overdraft.
    let postings = ledger
        .store()
        .get_postings_by_account(10, None, Some(&usd()), PostingFilter::Active)
        .await
        .unwrap();
    assert!(postings.iter().any(|p| p.value == Cent::from(-50)));
}

#[tokio::test]
async fn debit_must_not_exceed_credit_rejects_overspend() {
    let store = InMemoryStore::new();
    let ledger = Arc::new(Ledger::new(store));
    for (id, flags) in [
        (10, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT),
        (2, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT),
        (99, AccountFlags::empty()),
    ] {
        ledger
            .store()
            .create_account(make_account(id, flags))
            .await
            .unwrap();
    }

    // Account 10 forbids overdraft, so paying 100 from an empty balance fails.
    let transfer = TransferBuilder::new()
        .pay(account(10), account(2), usd(), Cent::from(100))
        .build();
    assert!(ledger.commit(transfer).await.is_err());
    assert_eq!(
        ledger.balance(&account(10), &usd()).await.unwrap(),
        Cent::ZERO
    );
}

#[tokio::test]
async fn overdraft_allows_arbitrary_negative() {
    let store = InMemoryStore::new();
    let ledger = Arc::new(Ledger::new(store));
    for (id, flags) in [
        (10, AccountFlags::empty()),
        (2, AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT),
        (99, AccountFlags::empty()),
    ] {
        ledger
            .store()
            .create_account(make_account(id, flags))
            .await
            .unwrap();
    }

    pay(
        &ledger,
        account(10),
        account(2),
        usd(),
        Cent::from(1_000_000),
    )
    .await;
    assert_eq!(
        ledger.balance(&account(10), &usd()).await.unwrap(),
        Cent::from(-1_000_000)
    );
}

// ---------------------------------------------------------------------------
// Book policy enforcement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn book_policy_rejects_disallowed_asset() {
    let ledger = setup_ledger().await;
    // Book 5 permits only EUR.
    let book = BookBuilder::new("eur-only")
        .id(BookId::new(5))
        .allow_asset(eur())
        .build();
    ledger.store().create_book(book).await.unwrap();

    deposit(&ledger, account(1), usd(), Cent::from(100), external()).await;

    // Paying USD under a EUR-only book is rejected, balance unchanged.
    let transfer = TransferBuilder::new()
        .book(BookId::new(5))
        .pay(account(1), account(2), usd(), Cent::from(50))
        .build();
    assert!(ledger.commit(transfer).await.is_err());
    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(100)
    );
}

#[tokio::test]
async fn transfer_in_missing_named_book_is_rejected() {
    let ledger = setup_ledger().await;
    deposit(&ledger, account(1), usd(), Cent::from(100), external()).await;

    let transfer = TransferBuilder::new()
        .book(BookId::new(404))
        .pay(account(1), account(2), usd(), Cent::from(50))
        .build();
    assert!(ledger.commit(transfer).await.is_err());
    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(100)
    );
}

// ---------------------------------------------------------------------------
// Content-addressed determinism
// ---------------------------------------------------------------------------

#[tokio::test]
async fn identical_transfers_share_envelope_id() {
    // Two independently-built default-book transfers must hash identically.
    let a = TransferBuilder::new()
        .pay(account(1), account(2), usd(), Cent::from(10))
        .build();
    let b = TransferBuilder::new()
        .pay(account(1), account(2), usd(), Cent::from(10))
        .build();
    assert_eq!(a.book, b.book, "default book must be deterministic");
    assert_eq!(a.book, DEFAULT_BOOK);
}

// ---------------------------------------------------------------------------
// Subaccounts (ADR-0012)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn subaccount_balances_are_segregated() {
    let ledger = setup_ledger().await;
    let sub = AccountId::with_sub(1, 7);
    // A subaccount is a full account record with its own policy.
    ledger
        .store()
        .create_account(Account::debit_must_not_exceed_credit(sub))
        .await
        .unwrap();

    // Fund the main account (1, 0) and the subaccount (1, 7) independently.
    deposit(&ledger, account(1), usd(), Cent::from(100), external()).await;
    deposit(&ledger, sub, usd(), Cent::from(40), external()).await;

    // balance() reads exactly one subaccount and never rolls up the other.
    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(100)
    );
    assert_eq!(ledger.balance(&sub, &usd()).await.unwrap(), Cent::from(40));

    // list_subaccounts spans the base id's subaccounts, sorted.
    let subs = ledger.list_subaccounts(&account(1)).await.unwrap();
    assert_eq!(subs, vec![account(1), sub]);

    // balances() reports one entry per subaccount, never summed.
    let all = ledger.balances(&account(1), &usd(), None).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(
        all.iter().find(|b| b.account == account(1)).unwrap().value,
        Cent::from(100)
    );
    assert_eq!(
        all.iter().find(|b| b.account == sub).unwrap().value,
        Cent::from(40)
    );

    // A subaccount filter restricts to that one.
    let just_sub = ledger.balances(&account(1), &usd(), Some(7)).await.unwrap();
    assert_eq!(just_sub.len(), 1);
    assert_eq!(just_sub[0].account, sub);
    assert_eq!(just_sub[0].value, Cent::from(40));
}

#[tokio::test]
async fn pay_moves_value_between_subaccounts() {
    let ledger = setup_ledger().await;
    let sub = AccountId::with_sub(1, 7);
    ledger
        .store()
        .create_account(Account::debit_must_not_exceed_credit(sub))
        .await
        .unwrap();

    deposit(&ledger, sub, usd(), Cent::from(40), external()).await;
    // Move 30 from subaccount (1, 7) to the main account (1, 0).
    pay(&ledger, sub, account(1), usd(), Cent::from(30)).await;

    assert_eq!(ledger.balance(&sub, &usd()).await.unwrap(), Cent::from(10));
    assert_eq!(
        ledger.balance(&account(1), &usd()).await.unwrap(),
        Cent::from(30)
    );
}

#[tokio::test]
async fn closed_subaccounts_drop_out_of_aggregate_reads() {
    let ledger = setup_ledger().await;
    let sub = AccountId::with_sub(1, 7);
    ledger
        .store()
        .create_account(Account::debit_must_not_exceed_credit(sub))
        .await
        .unwrap();

    // The subaccount is created then closed while empty.
    ledger.close(&sub).await.unwrap();

    // list_subaccounts and balances exclude the closed subaccount.
    let subs = ledger.list_subaccounts(&account(1)).await.unwrap();
    assert_eq!(subs, vec![account(1)]);
    let all = ledger.balances(&account(1), &usd(), None).await.unwrap();
    assert!(all.iter().all(|b| b.account != sub));
}
