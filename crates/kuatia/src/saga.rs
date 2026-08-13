//! Multi-transfer composition over the ledger.
//!
//! [`run_movements`] commits a sequence of transfers in order and, on the first
//! failure, reverses the already-committed ones in LIFO order via
//! [`Ledger::reverse`], so several transfers behave as one all-or-nothing
//! workflow (an FX trade, a multi-leg settlement). Each single commit is already
//! atomic in the store (ADR-0023); this adds only the cross-transfer unwind,
//! which is why it no longer needs a saga VM.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use kuatia_core::{AccountId, AssetId, Cent, Receipt, Transfer, TransferBuilder};

use crate::error::LedgerError;
use crate::ledger::Ledger;

/// A pay movement between two accounts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PayInput {
    /// Source account.
    pub from: AccountId,
    /// Destination account.
    pub to: AccountId,
    /// Asset to transfer.
    pub asset: AssetId,
    /// Amount to transfer.
    pub amount: Cent,
}

/// A deposit from an external account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepositInput {
    /// Account receiving the deposit.
    pub to: AccountId,
    /// Asset being deposited.
    pub asset: AssetId,
    /// Amount to deposit.
    pub amount: Cent,
    /// External account funding the deposit.
    pub external: AccountId,
}

/// One leg of a multi-transfer workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Movement {
    /// Pay between two accounts.
    Pay(PayInput),
    /// Deposit from an external account.
    Deposit(DepositInput),
}

impl Movement {
    /// Build the concrete [`Transfer`] this movement commits.
    fn build(&self) -> Result<Transfer, LedgerError> {
        match self {
            Movement::Pay(p) => Ok(TransferBuilder::new()
                .pay(p.from, p.to, p.asset, p.amount)
                .build()),
            Movement::Deposit(d) => Ok(TransferBuilder::new()
                .deposit(d.to, d.asset, d.amount, d.external)?
                .build()),
        }
    }
}

/// Commit each movement in order, returning the receipts. On the first failure,
/// reverse the already-committed receipts in LIFO order and return the original
/// error. If a reversal itself fails, return [`LedgerError::CompensationFailed`]
/// carrying both the original and the compensation error.
pub async fn run_movements(
    ledger: &Arc<Ledger>,
    movements: &[Movement],
) -> Result<Vec<Receipt>, LedgerError> {
    let mut receipts: Vec<Receipt> = Vec::new();
    for movement in movements {
        let transfer = movement.build()?;
        match ledger.commit(transfer).await {
            Ok(receipt) => receipts.push(receipt),
            Err(err) => {
                for receipt in receipts.iter().rev() {
                    if let Err(compensation) = ledger.reverse(&receipt.transfer_id).await {
                        return Err(LedgerError::CompensationFailed {
                            original: Box::new(err),
                            compensation: Box::new(compensation),
                        });
                    }
                }
                return Err(err);
            }
        }
    }
    Ok(receipts)
}
