//! Pure intent resolution: turn a [`Transfer`] (movements) into a concrete
//! [`Envelope`] (postings to consume and create).
//!
//! Resolution is two passes, both sans-IO and deterministic:
//!
//! 1. [`draft_movements`] aggregates movements into output postings and
//!    per-(account, asset) net debits. It tells the async layer exactly which
//!    postings and account policies to load.
//! 2. [`resolve_envelope`] selects postings for each debit, computes change, and
//!    covers an overdraft shortfall with a negative offset posting.
//!
//! The async ledger loads state; this module decides. The change-making and
//! shortfall branches are the parts most worth property-testing, and living here
//! they are reachable without standing up a store.

use std::collections::HashMap;

use crate::posting_selection::SelectionError;
use kuatia_types::{
    AccountId, AccountPolicy, AssetId, Cent, Envelope, EnvelopeBuilder, NewPosting, OverflowError,
    Posting, PostingId, Transfer,
};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure from resolution pass 2 ([`resolve_envelope`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// Posting selection failed: insufficient funds for a non-overdraft account,
    /// or an overflow while summing candidate postings.
    Selection(SelectionError),
    /// Monetary arithmetic overflowed while computing change or a shortfall.
    Overflow,
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Selection(e) => write!(f, "selection: {e}"),
            Self::Overflow => write!(f, "monetary amount overflow"),
        }
    }
}

impl std::error::Error for ResolveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Selection(e) => Some(e),
            Self::Overflow => None,
        }
    }
}

impl From<SelectionError> for ResolveError {
    fn from(e: SelectionError) -> Self {
        Self::Selection(e)
    }
}

impl From<OverflowError> for ResolveError {
    fn from(_: OverflowError) -> Self {
        Self::Overflow
    }
}

// ---------------------------------------------------------------------------
// Pass 1: draft
// ---------------------------------------------------------------------------

/// A positive net debit on one (account, asset) that pass 2 must cover by
/// selecting postings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Debit {
    /// The account (subaccount) being debited.
    pub account: AccountId,
    /// The asset owed.
    pub asset: AssetId,
    /// The positive amount owed.
    pub amount: Cent,
}

/// Output of resolution pass 1: postings credited straight from movements, plus
/// the debits that still require posting selection in pass 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovementDraft {
    /// Postings credited to movement destinations.
    pub creates: Vec<NewPosting>,
    /// Positive net debits, one per (account, asset), each needing selection.
    pub debits: Vec<Debit>,
}

/// Pass 1: for each movement create an output posting on its destination and
/// accumulate the net debit on its source. Debits are returned in a
/// deterministic (account, asset) order so golden vectors are stable regardless
/// of `HashMap` iteration order.
pub fn draft_movements(transfer: &Transfer) -> Result<MovementDraft, OverflowError> {
    let mut creates: Vec<NewPosting> = Vec::new();
    let mut net_debits: HashMap<(AccountId, AssetId), Cent> = HashMap::new();

    for m in &transfer.movements {
        let payer = if m.from != m.to { Some(m.from) } else { None };
        creates.push(NewPosting {
            owner: m.to,
            asset: m.asset,
            value: m.amount,
            payer,
        });
        let entry = net_debits.entry((m.from, m.asset)).or_insert(Cent::ZERO);
        *entry = entry.checked_add(m.amount)?;
    }

    let mut debits: Vec<Debit> = net_debits
        .into_iter()
        .filter(|(_, amount)| amount.is_positive())
        .map(|((account, asset), amount)| Debit {
            account,
            asset,
            amount,
        })
        .collect();
    debits.sort_by_key(|d| (d.account, d.asset));

    Ok(MovementDraft { creates, debits })
}

// ---------------------------------------------------------------------------
// Pass 2: resolve
// ---------------------------------------------------------------------------

/// Pre-loaded state for resolution pass 2. The async layer gathers `available`
/// and `policies` for the debits produced by [`draft_movements`]; this pass is
/// pure.
pub struct ResolveInput<'a> {
    /// The transfer being resolved (for its book and metadata).
    pub transfer: &'a Transfer,
    /// Pass 1 output. Consumed here: its `creates` seed the envelope and its
    /// `debits` drive selection.
    pub draft: MovementDraft,
    /// Active postings available for each debit's (account, asset). A missing or
    /// empty entry means no positive postings to draw on.
    pub available: &'a HashMap<(AccountId, AssetId), Vec<Posting>>,
    /// Policy for each debit's account. A missing entry is treated as "no
    /// overdraft" — the debit fails with [`SelectionError::InsufficientFunds`]
    /// rather than granting an offset position on unknown terms.
    pub policies: &'a HashMap<AccountId, AccountPolicy>,
}

/// Pass 2: for each debit, either select postings and compute change, or (for an
/// overdraft account short of funds) consume every positive posting and create a
/// negative offset posting for the shortfall. The floor is enforced later, in
/// validation. Returns the concrete envelope with no account snapshots pinned;
/// the caller pins them.
pub fn resolve_envelope(input: ResolveInput<'_>) -> Result<Envelope, ResolveError> {
    let ResolveInput {
        transfer,
        draft,
        available,
        policies,
    } = input;
    let MovementDraft {
        mut creates,
        debits,
    } = draft;
    let mut consumes: Vec<PostingId> = Vec::new();

    for debit in &debits {
        let avail: &[Posting] = available
            .get(&(debit.account, debit.asset))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let total_positive = Cent::checked_sum(
            avail
                .iter()
                .filter(|p| p.value.is_positive())
                .map(|p| p.value),
        )?;

        if total_positive >= debit.amount {
            // Enough positive postings: greedily take them largest-first to
            // minimise the number consumed (and therefore the change postings
            // created), summing as we go so we stop once the debit is covered.
            let mut candidates: Vec<&Posting> =
                avail.iter().filter(|p| p.value.is_positive()).collect();
            candidates.sort_by_key(|p| std::cmp::Reverse(p.value));

            let mut consumed_sum = Cent::ZERO;
            for posting in candidates {
                consumes.push(posting.id);
                consumed_sum = consumed_sum.checked_add(posting.value)?;
                if consumed_sum >= debit.amount {
                    break;
                }
            }

            let change = consumed_sum.checked_sub(debit.amount)?;
            if change.is_positive() {
                creates.push(NewPosting {
                    owner: debit.account,
                    asset: debit.asset,
                    value: change,
                    payer: None,
                });
            }
        } else {
            // Not enough positive postings. Overdraft accounts cover the
            // shortfall with a negative posting (an offset position); any other
            // policy — or an unknown one — fails.
            match policies.get(&debit.account) {
                Some(AccountPolicy::CappedOverdraft { .. } | AccountPolicy::UncappedOverdraft) => {
                    let positives: Vec<PostingId> = avail
                        .iter()
                        .filter(|p| p.value.is_positive())
                        .map(|p| p.id)
                        .collect();
                    consumes.extend_from_slice(&positives);
                    let shortfall = debit.amount.checked_sub(total_positive)?;
                    creates.push(NewPosting {
                        owner: debit.account,
                        asset: debit.asset,
                        value: shortfall.checked_neg()?,
                        payer: None,
                    });
                }
                _ => {
                    return Err(ResolveError::Selection(SelectionError::InsufficientFunds {
                        available: total_positive,
                        requested: debit.amount,
                    }));
                }
            }
        }
    }

    Ok(EnvelopeBuilder::new()
        .consumes(consumes)
        .creates(creates)
        .book(transfer.book)
        .metadata(transfer.metadata.clone())
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuatia_types::*;

    fn acct(id: i64) -> AccountId {
        AccountId::new(id)
    }

    fn posting(owner: AccountId, index: u16, value: i64) -> Posting {
        Posting::new(
            PostingId {
                transfer: EnvelopeId([index as u8; 32]),
                index,
            },
            owner,
            AssetId::new(1),
            Cent::from(value),
        )
    }

    fn pay(from: AccountId, to: AccountId, amount: i64) -> Transfer {
        TransferBuilder::new()
            .pay(from, to, AssetId::new(1), Cent::from(amount))
            .build()
    }

    #[test]
    fn draft_aggregates_net_debit_and_output() {
        let draft = draft_movements(&pay(acct(1), acct(2), 100)).unwrap();
        assert_eq!(draft.creates.len(), 1);
        assert_eq!(draft.creates[0].owner, acct(2));
        assert_eq!(draft.creates[0].value, Cent::from(100));
        assert_eq!(draft.creates[0].payer, Some(acct(1)));
        assert_eq!(
            draft.debits,
            vec![Debit {
                account: acct(1),
                asset: AssetId::new(1),
                amount: Cent::from(100),
            }]
        );
    }

    #[test]
    fn self_movement_has_no_payer_but_still_debits() {
        // from == to: the created posting carries no payer, but the source still
        // owes its own net debit (the credit is a separate created posting, not
        // an offset), so a debit is produced.
        let draft = draft_movements(&pay(acct(1), acct(1), 100)).unwrap();
        assert_eq!(draft.creates[0].payer, None);
        assert_eq!(
            draft.debits,
            vec![Debit {
                account: acct(1),
                asset: AssetId::new(1),
                amount: Cent::from(100),
            }]
        );
    }

    #[test]
    fn exact_funds_no_change() {
        let transfer = pay(acct(1), acct(2), 100);
        let draft = draft_movements(&transfer).unwrap();
        let available = HashMap::from([(
            (acct(1), AssetId::new(1)),
            vec![posting(acct(1), 0, 60), posting(acct(1), 1, 40)],
        )]);
        let policies = HashMap::new();
        let env = resolve_envelope(ResolveInput {
            transfer: &transfer,
            draft,
            available: &available,
            policies: &policies,
        })
        .unwrap();
        assert_eq!(env.consumes().len(), 2);
        // Only the destination posting is created — no change.
        assert_eq!(env.creates().len(), 1);
        assert_eq!(env.creates()[0].owner, acct(2));
    }

    #[test]
    fn overpay_creates_change_posting() {
        let transfer = pay(acct(1), acct(2), 30);
        let draft = draft_movements(&transfer).unwrap();
        let available =
            HashMap::from([((acct(1), AssetId::new(1)), vec![posting(acct(1), 0, 100)])]);
        let policies = HashMap::new();
        let env = resolve_envelope(ResolveInput {
            transfer: &transfer,
            draft,
            available: &available,
            policies: &policies,
        })
        .unwrap();
        assert_eq!(env.consumes().len(), 1);
        // Destination posting + a change posting back to the payer.
        let change: Vec<_> = env
            .creates()
            .iter()
            .filter(|p| p.owner == acct(1))
            .collect();
        assert_eq!(change.len(), 1);
        assert_eq!(change[0].value, Cent::from(70));
    }

    #[test]
    fn selects_largest_first_to_minimise_consumed() {
        // With 10, 90 and 50 available, a debit of 80 is covered by the single
        // 90 posting rather than several smaller ones — one consumed, 10 change.
        let transfer = pay(acct(1), acct(2), 80);
        let draft = draft_movements(&transfer).unwrap();
        let available = HashMap::from([(
            (acct(1), AssetId::new(1)),
            vec![
                posting(acct(1), 0, 10),
                posting(acct(1), 1, 90),
                posting(acct(1), 2, 50),
            ],
        )]);
        let policies = HashMap::new();
        let env = resolve_envelope(ResolveInput {
            transfer: &transfer,
            draft,
            available: &available,
            policies: &policies,
        })
        .unwrap();
        assert_eq!(env.consumes().len(), 1);
        let change: Vec<_> = env
            .creates()
            .iter()
            .filter(|p| p.owner == acct(1))
            .collect();
        assert_eq!(change.len(), 1);
        assert_eq!(change[0].value, Cent::from(10));
    }

    #[test]
    fn insufficient_funds_without_overdraft_fails() {
        let transfer = pay(acct(1), acct(2), 100);
        let draft = draft_movements(&transfer).unwrap();
        let available =
            HashMap::from([((acct(1), AssetId::new(1)), vec![posting(acct(1), 0, 40)])]);
        let policies = HashMap::from([(acct(1), AccountPolicy::NoOverdraft)]);
        let err = resolve_envelope(ResolveInput {
            transfer: &transfer,
            draft,
            available: &available,
            policies: &policies,
        })
        .unwrap_err();
        assert_eq!(
            err,
            ResolveError::Selection(SelectionError::InsufficientFunds {
                available: Cent::from(40),
                requested: Cent::from(100),
            })
        );
    }

    #[test]
    fn missing_policy_is_treated_as_no_overdraft() {
        let transfer = pay(acct(1), acct(2), 100);
        let draft = draft_movements(&transfer).unwrap();
        let available = HashMap::new();
        let policies = HashMap::new();
        let err = resolve_envelope(ResolveInput {
            transfer: &transfer,
            draft,
            available: &available,
            policies: &policies,
        })
        .unwrap_err();
        assert_eq!(
            err,
            ResolveError::Selection(SelectionError::InsufficientFunds {
                available: Cent::ZERO,
                requested: Cent::from(100),
            })
        );
    }

    #[test]
    fn overdraft_covers_shortfall_with_offset_posting() {
        let transfer = pay(acct(1), acct(2), 100);
        let draft = draft_movements(&transfer).unwrap();
        let available =
            HashMap::from([((acct(1), AssetId::new(1)), vec![posting(acct(1), 0, 30)])]);
        let policies = HashMap::from([(acct(1), AccountPolicy::UncappedOverdraft)]);
        let env = resolve_envelope(ResolveInput {
            transfer: &transfer,
            draft,
            available: &available,
            policies: &policies,
        })
        .unwrap();
        // The single positive posting is consumed.
        assert_eq!(env.consumes().len(), 1);
        // A negative offset posting covers the 70 shortfall on the payer.
        let offset: Vec<_> = env
            .creates()
            .iter()
            .filter(|p| p.owner == acct(1))
            .collect();
        assert_eq!(offset.len(), 1);
        assert_eq!(offset[0].value, Cent::from(-70));
    }

    #[test]
    fn capped_overdraft_covers_shortfall() {
        // The floor is enforced in validation, not here — resolve still creates
        // the offset posting for a capped-overdraft account.
        let transfer = pay(acct(1), acct(2), 100);
        let draft = draft_movements(&transfer).unwrap();
        let available = HashMap::new();
        let policies = HashMap::from([(
            acct(1),
            AccountPolicy::CappedOverdraft {
                floor: Cent::from(-1000),
            },
        )]);
        let env = resolve_envelope(ResolveInput {
            transfer: &transfer,
            draft,
            available: &available,
            policies: &policies,
        })
        .unwrap();
        assert!(env.consumes().is_empty());
        assert_eq!(
            env.creates()
                .iter()
                .find(|p| p.owner == acct(1))
                .unwrap()
                .value,
            Cent::from(-100)
        );
    }
}
