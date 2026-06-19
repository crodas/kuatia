//! Pure, sync validation — the auditable heart of the ledger.
//!
//! [`validate_and_plan`] enforces every invariant (conservation, double-spend,
//! ownership, account policy) and produces a [`Plan`] describing the effects to
//! apply. It takes no IO, no clock, and no randomness, so it is deterministic
//! and testable with golden vectors. The caller provides pre-loaded state via
//! [`PlanInput`]; this module never touches storage.

use std::collections::{HashMap, HashSet};

use crate::hash::{account_hash, envelope_id};
use kuatia_types::*;

// ---------------------------------------------------------------------------
// Input / Output
// ---------------------------------------------------------------------------

/// Pre-loaded state the caller must supply. Borrowing avoids copies on the
/// hot path and keeps this module allocation-free for the validation itself.
pub struct PlanInput<'a> {
    /// The envelope to validate.
    pub envelope: &'a Envelope,
    /// Postings referenced by `transfer.consumes`.
    pub consumed_postings: &'a [Posting],
    /// All accounts referenced by the transfer.
    pub accounts: &'a HashMap<AccountId, Account>,
    /// Current balances keyed by (account, asset).
    pub balances: &'a HashMap<(AccountId, AssetId), Cent>,
    /// The book gating this transfer, if one is loaded. `Some` enforces the
    /// book's [`BookPolicy`] (allowed assets/accounts/flags); `None` means the
    /// implicit unrestricted default book. The async layer is responsible for
    /// rejecting a *named* book id that has no row before reaching here.
    pub book: Option<&'a Book>,
}

/// The validated effects to apply atomically. Produced only when every
/// invariant holds, so the store can apply it without re-checking.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Plan {
    /// Content-addressed id of the validated transfer.
    pub transfer_id: EnvelopeId,
    /// Postings to mark as inactive (consumed).
    pub postings_to_deactivate: Vec<PostingId>,
    /// New postings to persist.
    pub postings_to_create: Vec<Posting>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// An invariant violation detected during transfer validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// Transfer has no consumptions and no creations.
    EmptyTransfer,
    /// The same posting id appears more than once in `consumes`.
    DuplicateConsumedPosting(PostingId),
    /// A consumed posting id does not exist in the store.
    PostingNotFound(PostingId),
    /// A consumed posting has already been spent.
    PostingAlreadyConsumed(PostingId),
    /// A consumed posting is not owned by the expected account.
    OwnershipViolation {
        /// The posting that failed the ownership check.
        posting_id: PostingId,
        /// The account that should own the posting.
        expected: AccountId,
        /// The account that actually owns the posting.
        actual: AccountId,
    },
    /// A referenced account does not exist.
    AccountNotFound(AccountId),
    /// A referenced account is frozen.
    AccountFrozen(AccountId),
    /// A referenced account is closed.
    AccountClosed(AccountId),
    /// Per-asset conservation law violated: consumed sum != created sum.
    ConservationViolation {
        /// The asset whose sums differ.
        asset: AssetId,
        /// Total value of consumed postings for this asset.
        consumed_sum: Cent,
        /// Total value of created postings for this asset.
        created_sum: Cent,
    },
    /// Projected balance would fall below the account's floor.
    OverdraftExceeded {
        /// The account that would be overdrawn.
        account: AccountId,
        /// The asset involved.
        asset: AssetId,
        /// The minimum allowed balance.
        floor: Cent,
        /// The balance that would result from this transfer.
        projected: Cent,
    },
    /// Account snapshot hash does not match current state (stale read).
    AccountVersionMismatch {
        /// The account whose version was stale.
        account: AccountId,
        /// The snapshot hash the transfer expected.
        expected: [u8; 32],
        /// The actual current snapshot hash.
        actual: [u8; 32],
    },
    /// A negative posting targets an account whose policy forbids offset positions.
    NegativePostingOnNonSystemAccount {
        /// The account that would receive the negative posting.
        account: AccountId,
        /// The asset involved.
        asset: AssetId,
        /// The negative value.
        value: Cent,
    },
    /// An asset is not permitted by the transfer's book policy.
    BookAssetNotAllowed {
        /// The book whose policy rejected the asset.
        book: BookId,
        /// The disallowed asset.
        asset: AssetId,
    },
    /// An account is not permitted to participate by the transfer's book policy.
    BookAccountNotAllowed {
        /// The book whose policy rejected the account.
        book: BookId,
        /// The disallowed account.
        account: AccountId,
    },
    /// An arithmetic operation overflowed.
    Overflow,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyTransfer => write!(f, "transfer has no postings"),
            Self::DuplicateConsumedPosting(id) => write!(f, "duplicate consumed posting {id:?}"),
            Self::PostingNotFound(id) => write!(f, "posting not found: {id:?}"),
            Self::PostingAlreadyConsumed(id) => write!(f, "posting already consumed: {id:?}"),
            Self::OwnershipViolation {
                posting_id,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "ownership violation on {posting_id:?}: expected {expected:?}, got {actual:?}"
                )
            }
            Self::AccountNotFound(id) => write!(f, "account not found: {id:?}"),
            Self::AccountFrozen(id) => write!(f, "account frozen: {id:?}"),
            Self::AccountClosed(id) => write!(f, "account closed: {id:?}"),
            Self::ConservationViolation {
                asset,
                consumed_sum,
                created_sum,
            } => {
                write!(
                    f,
                    "conservation violated for {asset:?}: consumed {consumed_sum}, created {created_sum}"
                )
            }
            Self::OverdraftExceeded {
                account,
                asset,
                floor,
                projected,
            } => {
                write!(
                    f,
                    "overdraft exceeded for {account:?}/{asset:?}: floor {floor}, projected {projected}"
                )
            }
            Self::AccountVersionMismatch {
                account,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "account version mismatch for {account:?}: expected {expected:02x?}, got {actual:02x?}"
                )
            }
            Self::NegativePostingOnNonSystemAccount {
                account,
                asset,
                value,
            } => {
                write!(
                    f,
                    "negative posting ({value}) on account {account:?}/{asset:?} whose policy forbids offsets"
                )
            }
            Self::BookAssetNotAllowed { book, asset } => {
                write!(f, "asset {asset:?} not allowed by book {book:?}")
            }
            Self::BookAccountNotAllowed { book, account } => {
                write!(f, "account {account:?} not allowed by book {book:?}")
            }
            Self::Overflow => write!(f, "monetary amount overflow"),
        }
    }
}

impl std::error::Error for ValidationError {}

impl From<OverflowError> for ValidationError {
    fn from(_: OverflowError) -> Self {
        Self::Overflow
    }
}

// ---------------------------------------------------------------------------
// The pure decision function
// ---------------------------------------------------------------------------

/// The single entry point for all ledger invariant checks.
///
/// Pure, sync, deterministic — no IO, no clock, no randomness — so the
/// invariants are testable with golden vectors and replay deterministically.
/// Returns a [`Plan`] only when every invariant holds; otherwise returns the
/// specific [`ValidationError`] that was violated.
pub fn validate_and_plan(input: PlanInput<'_>) -> Result<Plan, ValidationError> {
    let envelope = input.envelope;

    // 1. Non-empty
    if envelope.consumes().is_empty() && envelope.creates().is_empty() {
        return Err(ValidationError::EmptyTransfer);
    }

    // 2. No duplicate consumed PostingIds
    {
        let mut seen = HashSet::with_capacity(envelope.consumes().len());
        for pid in envelope.consumes() {
            if !seen.insert(pid) {
                return Err(ValidationError::DuplicateConsumedPosting(*pid));
            }
        }
    }

    // Index consumed postings by id for lookup
    let consumed_by_id: HashMap<PostingId, &Posting> =
        input.consumed_postings.iter().map(|p| (p.id, p)).collect();

    // 3 & 4. Every consumed posting exists, is active, and we note ownership
    for pid in envelope.consumes() {
        let posting = consumed_by_id
            .get(pid)
            .ok_or(ValidationError::PostingNotFound(*pid))?;
        if posting.status != PostingStatus::Active
            && posting.status != PostingStatus::PendingInactive
        {
            return Err(ValidationError::PostingAlreadyConsumed(*pid));
        }
    }

    // 5. Every referenced account exists, not FROZEN, not CLOSED
    let mut all_account_ids: Vec<AccountId> = envelope.creates().iter().map(|p| p.owner).collect();
    for pid in envelope.consumes() {
        let posting = consumed_by_id[pid];
        all_account_ids.push(posting.owner);
    }
    all_account_ids.sort();
    all_account_ids.dedup();

    for aid in &all_account_ids {
        let account = input
            .accounts
            .get(aid)
            .ok_or(ValidationError::AccountNotFound(*aid))?;
        if account.is_frozen() {
            return Err(ValidationError::AccountFrozen(*aid));
        }
        if account.is_closed() {
            return Err(ValidationError::AccountClosed(*aid));
        }
    }

    // 5b. Snapshot pinning: each account_snapshot must match current state.
    for snap in envelope.account_snapshots() {
        let account = input
            .accounts
            .get(&snap.account)
            .ok_or(ValidationError::AccountNotFound(snap.account))?;
        let actual = account_hash(account);
        if snap.snapshot_id != actual {
            return Err(ValidationError::AccountVersionMismatch {
                account: snap.account,
                expected: snap.snapshot_id,
                actual,
            });
        }
    }

    // 5c. Book policy: gate which assets and accounts may participate. Enforced
    //     only when a book is loaded; an empty policy field means "no restriction".
    if let Some(book) = input.book {
        let policy = &book.policy;

        if !policy.allowed_assets.is_empty() {
            let mut referenced_assets: HashSet<AssetId> = HashSet::new();
            for pid in envelope.consumes() {
                referenced_assets.insert(consumed_by_id[pid].asset);
            }
            for np in envelope.creates() {
                referenced_assets.insert(np.asset);
            }
            for asset in &referenced_assets {
                if !policy.allowed_assets.contains(asset) {
                    return Err(ValidationError::BookAssetNotAllowed {
                        book: book.id,
                        asset: *asset,
                    });
                }
            }
        }

        let no_account_restriction =
            policy.allowed_accounts.is_empty() && policy.allowed_flags.is_empty();
        if !no_account_restriction {
            for aid in &all_account_ids {
                let account = &input.accounts[aid];
                let listed = policy.allowed_accounts.contains(aid);
                let flag_match = !policy.allowed_flags.is_empty()
                    && account.flags.intersects(policy.allowed_flags);
                if !(listed || flag_match) {
                    return Err(ValidationError::BookAccountNotAllowed {
                        book: book.id,
                        account: *aid,
                    });
                }
            }
        }
    }

    // 6. Per-asset conservation: Σ consumed == Σ created
    let mut consumed_by_asset: HashMap<AssetId, Cent> = HashMap::new();
    for pid in envelope.consumes() {
        let posting = consumed_by_id[pid];
        let entry = consumed_by_asset.entry(posting.asset).or_insert(Cent::ZERO);
        *entry = entry.checked_add(posting.value)?;
    }

    let mut created_by_asset: HashMap<AssetId, Cent> = HashMap::new();
    for np in envelope.creates() {
        let entry = created_by_asset.entry(np.asset).or_insert(Cent::ZERO);
        *entry = entry.checked_add(np.value)?;
    }

    // All assets must appear in both sides (or have sum 0 on the missing side)
    let mut all_assets: HashSet<AssetId> = HashSet::new();
    all_assets.extend(consumed_by_asset.keys());
    all_assets.extend(created_by_asset.keys());

    for asset in &all_assets {
        let consumed_sum = consumed_by_asset.get(asset).copied().unwrap_or(Cent::ZERO);
        let created_sum = created_by_asset.get(asset).copied().unwrap_or(Cent::ZERO);
        if consumed_sum != created_sum {
            return Err(ValidationError::ConservationViolation {
                asset: *asset,
                consumed_sum,
                created_sum,
            });
        }
    }

    // 7. Negative postings (offset positions) may target system, external, or
    //    overdraft accounts. Overdraft floors are enforced separately in step 8.
    //    Only NoOverdraft forbids holding a negative posting.
    for np in envelope.creates() {
        if np.value.is_negative() {
            let account = input
                .accounts
                .get(&np.owner)
                .ok_or(ValidationError::AccountNotFound(np.owner))?;
            match account.policy {
                AccountPolicy::SystemAccount
                | AccountPolicy::ExternalAccount
                | AccountPolicy::UncappedOverdraft
                | AccountPolicy::CappedOverdraft { .. } => {}
                AccountPolicy::NoOverdraft => {
                    return Err(ValidationError::NegativePostingOnNonSystemAccount {
                        account: np.owner,
                        asset: np.asset,
                        value: np.value,
                    });
                }
            }
        }
    }

    // 8. Policy: projected balance satisfies account's floor
    let mut deltas: HashMap<(AccountId, AssetId), Cent> = HashMap::new();
    for pid in envelope.consumes() {
        let posting = consumed_by_id[pid];
        let entry = deltas
            .entry((posting.owner, posting.asset))
            .or_insert(Cent::ZERO);
        *entry = entry.checked_sub(posting.value)?;
    }
    for np in envelope.creates() {
        let entry = deltas.entry((np.owner, np.asset)).or_insert(Cent::ZERO);
        *entry = entry.checked_add(np.value)?;
    }

    for ((account_id, asset_id), delta) in &deltas {
        let current_balance = input
            .balances
            .get(&(*account_id, *asset_id))
            .copied()
            .unwrap_or(Cent::ZERO);
        let projected = current_balance.checked_add(*delta)?;

        let account = &input.accounts[account_id];
        match &account.policy {
            AccountPolicy::NoOverdraft => {
                if projected.is_negative() {
                    return Err(ValidationError::OverdraftExceeded {
                        account: *account_id,
                        asset: *asset_id,
                        floor: Cent::ZERO,
                        projected,
                    });
                }
            }
            AccountPolicy::CappedOverdraft { floor } => {
                if projected < *floor {
                    return Err(ValidationError::OverdraftExceeded {
                        account: *account_id,
                        asset: *asset_id,
                        floor: *floor,
                        projected,
                    });
                }
            }
            AccountPolicy::UncappedOverdraft
            | AccountPolicy::SystemAccount
            | AccountPolicy::ExternalAccount => {
                // No floor check
            }
        }
    }

    // 8. Build the plan
    let tid = envelope_id(envelope);

    let postings_to_deactivate: Vec<PostingId> = envelope.consumes().to_vec();

    let postings_to_create: Vec<Posting> = envelope
        .creates
        .iter()
        .enumerate()
        .map(|(i, np)| {
            Posting::new(
                PostingId {
                    transfer: tid,
                    index: i as u16,
                },
                np.owner,
                np.asset,
                np.value,
            )
        })
        .collect();

    Ok(Plan {
        transfer_id: tid,
        postings_to_deactivate,
        postings_to_create,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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

    fn accounts_map(accs: Vec<Account>) -> HashMap<AccountId, Account> {
        accs.into_iter().map(|a| (a.id, a)).collect()
    }

    // -- Deposit: external(-100) + account1(+100) --------------------------

    fn deposit_envelope() -> Envelope {
        Envelope {
            consumes: vec![],
            creates: vec![
                NewPosting {
                    owner: AccountId::new(1),
                    asset: AssetId::new(1),
                    value: Cent::from(100),
                    payer: None,
                },
                NewPosting {
                    owner: AccountId::new(99),
                    asset: AssetId::new(1),
                    value: Cent::from(-100),
                    payer: None,
                },
            ],
            book: BookId(0),
            user_data: UserData::default(),
            account_snapshots: vec![],
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn valid_deposit() {
        let envelope = deposit_envelope();
        let accounts = accounts_map(vec![
            make_account(1, AccountPolicy::NoOverdraft),
            make_account(99, AccountPolicy::ExternalAccount),
        ]);
        let balances = HashMap::new();
        let input = PlanInput {
            envelope: &envelope,
            consumed_postings: &[],
            accounts: &accounts,
            balances: &balances,
            book: None,
        };

        let plan = validate_and_plan(input).unwrap();
        assert_eq!(plan.postings_to_create.len(), 2);
        assert!(plan.postings_to_deactivate.is_empty());
    }

    #[test]
    fn empty_transfer_rejected() {
        let envelope = Envelope {
            consumes: vec![],
            creates: vec![],
            book: BookId(0),
            user_data: UserData::default(),
            account_snapshots: vec![],
            metadata: BTreeMap::new(),
        };
        let accounts = HashMap::new();
        let balances = HashMap::new();
        let input = PlanInput {
            envelope: &envelope,
            consumed_postings: &[],
            accounts: &accounts,
            balances: &balances,
            book: None,
        };

        assert_eq!(
            validate_and_plan(input).unwrap_err(),
            ValidationError::EmptyTransfer
        );
    }

    #[test]
    fn conservation_violation() {
        let envelope = Envelope {
            consumes: vec![],
            creates: vec![NewPosting {
                owner: AccountId::new(1),
                asset: AssetId::new(1),
                value: Cent::from(100),
                payer: None,
            }],
            book: BookId(0),
            user_data: UserData::default(),
            account_snapshots: vec![],
            metadata: BTreeMap::new(),
        };
        let accounts = accounts_map(vec![make_account(1, AccountPolicy::NoOverdraft)]);
        let balances = HashMap::new();
        let input = PlanInput {
            envelope: &envelope,
            consumed_postings: &[],
            accounts: &accounts,
            balances: &balances,
            book: None,
        };

        match validate_and_plan(input) {
            Err(ValidationError::ConservationViolation { .. }) => {}
            other => panic!("expected ConservationViolation, got {other:?}"),
        }
    }

    #[test]
    fn posting_not_found() {
        let missing_pid = PostingId {
            transfer: EnvelopeId([0; 32]),
            index: 0,
        };
        let envelope = Envelope {
            consumes: vec![missing_pid],
            creates: vec![],
            book: BookId(0),
            user_data: UserData::default(),
            account_snapshots: vec![],
            metadata: BTreeMap::new(),
        };
        let accounts = HashMap::new();
        let balances = HashMap::new();
        let input = PlanInput {
            envelope: &envelope,
            consumed_postings: &[],
            accounts: &accounts,
            balances: &balances,
            book: None,
        };

        assert_eq!(
            validate_and_plan(input).unwrap_err(),
            ValidationError::PostingNotFound(missing_pid)
        );
    }

    #[test]
    fn double_spend_rejected() {
        let pid = PostingId {
            transfer: EnvelopeId([1; 32]),
            index: 0,
        };
        let posting = Posting {
            id: pid,
            owner: AccountId::new(1),
            asset: AssetId::new(1),
            value: Cent::from(100),
            status: PostingStatus::Inactive, // already consumed
            reservation: None,
        };
        let envelope = Envelope {
            consumes: vec![pid],
            creates: vec![NewPosting {
                owner: AccountId::new(2),
                asset: AssetId::new(1),
                value: Cent::from(100),
                payer: None,
            }],
            book: BookId(0),
            user_data: UserData::default(),
            account_snapshots: vec![],
            metadata: BTreeMap::new(),
        };
        let accounts = accounts_map(vec![
            make_account(1, AccountPolicy::NoOverdraft),
            make_account(2, AccountPolicy::NoOverdraft),
        ]);
        let balances = HashMap::new();
        let input = PlanInput {
            envelope: &envelope,
            consumed_postings: &[posting],
            accounts: &accounts,
            balances: &balances,
            book: None,
        };

        assert_eq!(
            validate_and_plan(input).unwrap_err(),
            ValidationError::PostingAlreadyConsumed(pid)
        );
    }

    #[test]
    fn account_frozen_rejected() {
        let envelope = deposit_envelope();
        let mut acc = make_account(1, AccountPolicy::NoOverdraft);
        acc.flags = AccountFlags::FROZEN;
        let accounts = accounts_map(vec![acc, make_account(99, AccountPolicy::ExternalAccount)]);
        let balances = HashMap::new();
        let input = PlanInput {
            envelope: &envelope,
            consumed_postings: &[],
            accounts: &accounts,
            balances: &balances,
            book: None,
        };

        assert_eq!(
            validate_and_plan(input).unwrap_err(),
            ValidationError::AccountFrozen(AccountId::new(1))
        );
    }

    #[test]
    fn account_closed_rejected() {
        let envelope = deposit_envelope();
        let mut acc = make_account(1, AccountPolicy::NoOverdraft);
        acc.flags = AccountFlags::CLOSED;
        let accounts = accounts_map(vec![acc, make_account(99, AccountPolicy::ExternalAccount)]);
        let balances = HashMap::new();
        let input = PlanInput {
            envelope: &envelope,
            consumed_postings: &[],
            accounts: &accounts,
            balances: &balances,
            book: None,
        };

        assert_eq!(
            validate_and_plan(input).unwrap_err(),
            ValidationError::AccountClosed(AccountId::new(1))
        );
    }

    #[test]
    fn no_overdraft_exceeded() {
        let pid = PostingId {
            transfer: EnvelopeId([1; 32]),
            index: 0,
        };
        let posting = Posting {
            id: pid,
            owner: AccountId::new(1),
            asset: AssetId::new(1),
            value: Cent::from(50),
            status: PostingStatus::Active,
            reservation: None,
        };
        // Try to send 50 but create 100 for recipient (conservation will fail first,
        // but let's test overdraft with a valid conservation)
        let envelope = Envelope {
            consumes: vec![pid],
            creates: vec![NewPosting {
                owner: AccountId::new(2),
                asset: AssetId::new(1),
                value: Cent::from(50),
                payer: None,
            }],
            book: BookId(0),
            user_data: UserData::default(),
            account_snapshots: vec![],
            metadata: BTreeMap::new(),
        };
        let accounts = accounts_map(vec![
            make_account(1, AccountPolicy::NoOverdraft),
            make_account(2, AccountPolicy::NoOverdraft),
        ]);
        // account1 has balance 50, consuming 50 leaves 0, that's fine.
        // Let's test when balance is insufficient: balance=30, consuming 50-value posting
        let mut balances = HashMap::new();
        balances.insert((AccountId::new(1), AssetId::new(1)), Cent::from(30));
        // projected = 30 - 50 = -20 < 0 → overdraft
        let input = PlanInput {
            envelope: &envelope,
            consumed_postings: &[posting],
            accounts: &accounts,
            balances: &balances,
            book: None,
        };

        match validate_and_plan(input) {
            Err(ValidationError::OverdraftExceeded { account, .. }) => {
                assert_eq!(account, AccountId::new(1));
            }
            other => panic!("expected OverdraftExceeded, got {other:?}"),
        }
    }

    #[test]
    fn capped_overdraft_within_limit() {
        let pid = PostingId {
            transfer: EnvelopeId([1; 32]),
            index: 0,
        };
        let posting = Posting {
            id: pid,
            owner: AccountId::new(1),
            asset: AssetId::new(1),
            value: Cent::from(100),
            status: PostingStatus::Active,
            reservation: None,
        };
        let envelope = Envelope {
            consumes: vec![pid],
            creates: vec![NewPosting {
                owner: AccountId::new(2),
                asset: AssetId::new(1),
                value: Cent::from(100),
                payer: None,
            }],
            book: BookId(0),
            user_data: UserData::default(),
            account_snapshots: vec![],
            metadata: BTreeMap::new(),
        };
        let accounts = accounts_map(vec![
            make_account(
                1,
                AccountPolicy::CappedOverdraft {
                    floor: Cent::from(-50),
                },
            ),
            make_account(2, AccountPolicy::NoOverdraft),
        ]);
        // balance=80, consuming 100 → projected = 80 - 100 = -20 >= -50 → OK
        let mut balances = HashMap::new();
        balances.insert((AccountId::new(1), AssetId::new(1)), Cent::from(80));

        let input = PlanInput {
            envelope: &envelope,
            consumed_postings: &[posting],
            accounts: &accounts,
            balances: &balances,
            book: None,
        };

        // A CappedOverdraft spend within the floor validates and produces a plan.
        let plan = validate_and_plan(input).unwrap();
        assert!(!plan.postings_to_create.is_empty());
    }

    #[test]
    fn capped_overdraft_exceeded() {
        let pid = PostingId {
            transfer: EnvelopeId([1; 32]),
            index: 0,
        };
        let posting = Posting {
            id: pid,
            owner: AccountId::new(1),
            asset: AssetId::new(1),
            value: Cent::from(100),
            status: PostingStatus::Active,
            reservation: None,
        };
        let envelope = Envelope {
            consumes: vec![pid],
            creates: vec![NewPosting {
                owner: AccountId::new(2),
                asset: AssetId::new(1),
                value: Cent::from(100),
                payer: None,
            }],
            book: BookId(0),
            user_data: UserData::default(),
            account_snapshots: vec![],
            metadata: BTreeMap::new(),
        };
        let accounts = accounts_map(vec![
            make_account(
                1,
                AccountPolicy::CappedOverdraft {
                    floor: Cent::from(-50),
                },
            ),
            make_account(2, AccountPolicy::NoOverdraft),
        ]);
        // balance=30, consuming 100 → projected = 30 - 100 = -70 < -50 → FAIL
        let mut balances = HashMap::new();
        balances.insert((AccountId::new(1), AssetId::new(1)), Cent::from(30));

        let input = PlanInput {
            envelope: &envelope,
            consumed_postings: &[posting],
            accounts: &accounts,
            balances: &balances,
            book: None,
        };

        match validate_and_plan(input) {
            Err(ValidationError::OverdraftExceeded {
                floor, projected, ..
            }) => {
                assert_eq!(floor, Cent::from(-50));
                assert_eq!(projected, Cent::from(-70));
            }
            other => panic!("expected OverdraftExceeded, got {other:?}"),
        }
    }

    #[test]
    fn uncapped_overdraft_allows_negative() {
        let pid = PostingId {
            transfer: EnvelopeId([1; 32]),
            index: 0,
        };
        let posting = Posting {
            id: pid,
            owner: AccountId::new(1),
            asset: AssetId::new(1),
            value: Cent::from(100),
            status: PostingStatus::Active,
            reservation: None,
        };
        let envelope = Envelope {
            consumes: vec![pid],
            creates: vec![NewPosting {
                owner: AccountId::new(2),
                asset: AssetId::new(1),
                value: Cent::from(100),
                payer: None,
            }],
            book: BookId(0),
            user_data: UserData::default(),
            account_snapshots: vec![],
            metadata: BTreeMap::new(),
        };
        let accounts = accounts_map(vec![
            make_account(1, AccountPolicy::UncappedOverdraft),
            make_account(2, AccountPolicy::NoOverdraft),
        ]);
        // balance=10, consuming 100 → projected = 10 - 100 = -90 → allowed
        let mut balances = HashMap::new();
        balances.insert((AccountId::new(1), AssetId::new(1)), Cent::from(10));

        let input = PlanInput {
            envelope: &envelope,
            consumed_postings: &[posting],
            accounts: &accounts,
            balances: &balances,
            book: None,
        };

        // UncappedOverdraft permits the negative projection; the plan validates.
        let plan = validate_and_plan(input).unwrap();
        assert!(!plan.postings_to_create.is_empty());
    }

    #[test]
    fn duplicate_consumed_posting_rejected() {
        let pid = PostingId {
            transfer: EnvelopeId([1; 32]),
            index: 0,
        };
        let envelope = Envelope {
            consumes: vec![pid, pid], // duplicate
            creates: vec![],
            book: BookId(0),
            user_data: UserData::default(),
            account_snapshots: vec![],
            metadata: BTreeMap::new(),
        };
        let accounts = HashMap::new();
        let balances = HashMap::new();
        let input = PlanInput {
            envelope: &envelope,
            consumed_postings: &[],
            accounts: &accounts,
            balances: &balances,
            book: None,
        };

        assert_eq!(
            validate_and_plan(input).unwrap_err(),
            ValidationError::DuplicateConsumedPosting(pid)
        );
    }

    #[test]
    fn internal_transfer_with_change() {
        // account1 has a 100 posting, sends 60 to account2, gets 40 change
        let pid = PostingId {
            transfer: EnvelopeId([1; 32]),
            index: 0,
        };
        let posting = Posting {
            id: pid,
            owner: AccountId::new(1),
            asset: AssetId::new(1),
            value: Cent::from(100),
            status: PostingStatus::Active,
            reservation: None,
        };
        let envelope = Envelope {
            consumes: vec![pid],
            creates: vec![
                NewPosting {
                    owner: AccountId::new(2),
                    asset: AssetId::new(1),
                    value: Cent::from(60),
                    payer: Some(AccountId::new(1)),
                },
                NewPosting {
                    owner: AccountId::new(1),
                    asset: AssetId::new(1),
                    value: Cent::from(40),
                    payer: None,
                },
            ],
            book: BookId(0),
            user_data: UserData::default(),
            account_snapshots: vec![],
            metadata: BTreeMap::new(),
        };
        let accounts = accounts_map(vec![
            make_account(1, AccountPolicy::NoOverdraft),
            make_account(2, AccountPolicy::NoOverdraft),
        ]);
        let mut balances = HashMap::new();
        balances.insert((AccountId::new(1), AssetId::new(1)), Cent::from(100));

        let input = PlanInput {
            envelope: &envelope,
            consumed_postings: &[posting],
            accounts: &accounts,
            balances: &balances,
            book: None,
        };

        let plan = validate_and_plan(input).unwrap();
        assert_eq!(plan.postings_to_deactivate.len(), 1);
        assert_eq!(plan.postings_to_create.len(), 2);
        // account1 projected: 100 - 100 + 40 = 40 >= 0 ✓
        // account2 projected: 0 + 60 = 60 >= 0 ✓
    }

    #[test]
    fn account_not_found() {
        let envelope = Envelope {
            consumes: vec![],
            creates: vec![
                NewPosting {
                    owner: AccountId::new(999),
                    asset: AssetId::new(1),
                    value: Cent::from(100),
                    payer: None,
                },
                NewPosting {
                    owner: AccountId::new(99),
                    asset: AssetId::new(1),
                    value: Cent::from(-100),
                    payer: None,
                },
            ],
            book: BookId(0),
            user_data: UserData::default(),
            account_snapshots: vec![],
            metadata: BTreeMap::new(),
        };
        // Only external account exists, account 999 doesn't
        let accounts = accounts_map(vec![make_account(99, AccountPolicy::ExternalAccount)]);
        let balances = HashMap::new();
        let input = PlanInput {
            envelope: &envelope,
            consumed_postings: &[],
            accounts: &accounts,
            balances: &balances,
            book: None,
        };

        assert_eq!(
            validate_and_plan(input).unwrap_err(),
            ValidationError::AccountNotFound(AccountId::new(999))
        );
    }

    #[test]
    fn negative_posting_rejected_on_regular_account() {
        let envelope = Envelope {
            consumes: vec![],
            creates: vec![
                NewPosting {
                    owner: AccountId::new(1),
                    asset: AssetId::new(1),
                    value: Cent::from(-100),
                    payer: None,
                },
                NewPosting {
                    owner: AccountId::new(1),
                    asset: AssetId::new(1),
                    value: Cent::from(100),
                    payer: None,
                },
            ],
            book: BookId(0),
            user_data: UserData::default(),
            account_snapshots: vec![],
            metadata: BTreeMap::new(),
        };
        let accounts = accounts_map(vec![make_account(1, AccountPolicy::NoOverdraft)]);
        let balances = HashMap::new();
        let input = PlanInput {
            envelope: &envelope,
            consumed_postings: &[],
            accounts: &accounts,
            balances: &balances,
            book: None,
        };

        assert_eq!(
            validate_and_plan(input).unwrap_err(),
            ValidationError::NegativePostingOnNonSystemAccount {
                account: AccountId::new(1),
                asset: AssetId::new(1),
                value: Cent::from(-100),
            }
        );
    }

    #[test]
    fn negative_posting_allowed_on_system_account() {
        let envelope = deposit_envelope();
        let accounts = accounts_map(vec![
            make_account(1, AccountPolicy::NoOverdraft),
            make_account(99, AccountPolicy::SystemAccount),
        ]);
        let balances = HashMap::new();
        let input = PlanInput {
            envelope: &envelope,
            consumed_postings: &[],
            accounts: &accounts,
            balances: &balances,
            book: None,
        };

        let plan = validate_and_plan(input).unwrap();
        assert_eq!(plan.postings_to_create.len(), 2);
    }
}
