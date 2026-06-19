//! Posting selection for the intent layer.
//!
//! When a caller uses `pay` or `withdraw`, they specify an amount — not which
//! postings to consume. This module picks the smallest set of postings that
//! covers the requested amount, so the intent layer can build the transfer
//! automatically without exposing UTXO mechanics to the caller.

use kuatia_types::{AssetId, Cent, Posting, PostingId};

/// Error returned when posting selection fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionError {
    /// Available postings do not cover the requested amount.
    InsufficientFunds {
        /// Total value of eligible postings.
        available: Cent,
        /// Amount the caller asked for.
        requested: Cent,
    },
    /// Summing posting values would overflow `Cent`.
    Overflow,
}

impl std::fmt::Display for SelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InsufficientFunds {
                available,
                requested,
            } => {
                write!(
                    f,
                    "insufficient funds: available {available}, requested {requested}"
                )
            }
            Self::Overflow => write!(f, "monetary amount overflow"),
        }
    }
}

impl std::error::Error for SelectionError {}

/// Picks postings to cover `target`, using largest-first greedy to minimise
/// the number of postings consumed (and therefore the number of change postings
/// created). Only active, positive postings of the right asset are considered.
pub fn select_postings(
    available: &[Posting],
    asset: AssetId,
    target: Cent,
) -> Result<Vec<PostingId>, SelectionError> {
    assert!(target.is_positive(), "target must be positive");

    let mut candidates: Vec<&Posting> = available
        .iter()
        .filter(|p| p.is_active() && p.asset == asset && p.value.is_positive())
        .collect();

    // Largest first
    candidates.sort_by_key(|p| std::cmp::Reverse(p.value));

    let mut total_available = Cent::ZERO;
    for p in &candidates {
        total_available = total_available
            .checked_add(p.value)
            .map_err(|_| SelectionError::Overflow)?;
    }
    if total_available < target {
        return Err(SelectionError::InsufficientFunds {
            available: total_available,
            requested: target,
        });
    }

    let mut selected = Vec::new();
    let mut sum = Cent::ZERO;
    for posting in candidates {
        selected.push(posting.id);
        sum = sum
            .checked_add(posting.value)
            .map_err(|_| SelectionError::Overflow)?;
        if sum >= target {
            break;
        }
    }

    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuatia_types::*;

    fn make_posting(index: u16, value: i64) -> Posting {
        Posting::new(
            PostingId {
                transfer: EnvelopeId([1; 32]),
                index,
            },
            AccountId::new(1),
            AssetId::new(1),
            Cent::from(value),
        )
    }

    #[test]
    fn exact_match() {
        let postings = vec![make_posting(0, 50), make_posting(1, 50)];
        let result = select_postings(&postings, AssetId::new(1), Cent::from(100)).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn largest_first() {
        let postings = vec![
            make_posting(0, 10),
            make_posting(1, 90),
            make_posting(2, 50),
        ];
        let result = select_postings(&postings, AssetId::new(1), Cent::from(80)).unwrap();
        // Should pick 90 first (enough on its own)
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].index, 1);
    }

    #[test]
    fn insufficient_funds() {
        let postings = vec![make_posting(0, 30), make_posting(1, 20)];
        let err = select_postings(&postings, AssetId::new(1), Cent::from(100)).unwrap_err();
        assert_eq!(
            err,
            SelectionError::InsufficientFunds {
                available: Cent::from(50),
                requested: Cent::from(100)
            }
        );
    }

    #[test]
    fn ignores_inactive_and_wrong_asset() {
        let mut inactive = make_posting(0, 1000);
        inactive.status = PostingStatus::Inactive;

        let mut wrong_asset = make_posting(1, 1000);
        wrong_asset.asset = AssetId::new(2);

        let good = make_posting(2, 50);

        let postings = vec![inactive, wrong_asset, good];
        let result = select_postings(&postings, AssetId::new(1), Cent::from(50)).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].index, 2);
    }

    #[test]
    fn ignores_negative_postings() {
        let negative = Posting::new(
            PostingId {
                transfer: EnvelopeId([1; 32]),
                index: 0,
            },
            AccountId::new(1),
            AssetId::new(1),
            Cent::from(-100),
        );
        let good = make_posting(1, 50);
        let postings = vec![negative, good];
        let result = select_postings(&postings, AssetId::new(1), Cent::from(50)).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].index, 1);
    }
}
