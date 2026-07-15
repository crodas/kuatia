//! Per-subaccount balance queries.
//!
//! Balances are always computed in Rust from the live (active or reserved)
//! postings and are never summed across subaccounts (ADR-0012).

use tracing::instrument;

use kuatia_core::{AccountId, AssetId, Cent, PostingFilter};

use super::Ledger;
use crate::error::LedgerError;

/// A single subaccount's balance for one asset. Balances are always reported
/// per subaccount and never summed across them (ADR-0012).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SubAccountBalance {
    /// The subaccount this balance belongs to.
    pub account: AccountId,
    /// The balance of `account` for the queried asset.
    pub value: Cent,
}

impl Ledger {
    /// Compute balance from the live (active or reserved) postings for an
    /// account/asset pair.
    pub(crate) async fn compute_balance(
        &self,
        account: &AccountId,
        asset: &AssetId,
    ) -> Result<Cent, LedgerError> {
        let postings = self
            .store
            .get_postings_by_account(
                account.id,
                Some(account.sub),
                Some(asset),
                PostingFilter::Live,
            )
            .await?;
        Ok(Cent::checked_sum(postings.iter().map(|p| p.value))?)
    }

    /// Query the current balance of one subaccount for a given asset. This reads
    /// exactly the `account` passed (base id and subaccount) and never rolls up
    /// other subaccounts.
    #[instrument(skip(self), name = "ledger.balance")]
    pub async fn balance(&self, account: &AccountId, asset: &AssetId) -> Result<Cent, LedgerError> {
        self.compute_balance(account, asset).await
    }

    /// Report the per-subaccount balances of a base account for one asset.
    ///
    /// One entry per non-closed subaccount. `sub == None` spans every
    /// subaccount of `account`'s base id; `Some(s)` restricts to that one.
    /// Balances are never summed across subaccounts (ADR-0012).
    #[instrument(skip(self), name = "ledger.balances")]
    pub async fn balances(
        &self,
        account: &AccountId,
        asset: &AssetId,
        sub: Option<i64>,
    ) -> Result<Vec<SubAccountBalance>, LedgerError> {
        let mut result = Vec::new();
        for subaccount in self.list_subaccounts(account).await? {
            if let Some(s) = sub
                && subaccount.sub != s
            {
                continue;
            }
            let value = self.compute_balance(&subaccount, asset).await?;
            result.push(SubAccountBalance {
                account: subaccount,
                value,
            });
        }
        Ok(result)
    }

    /// List the non-closed subaccounts of a base account.
    ///
    /// This scans every account row and filters in memory, so it pays for
    /// subaccounts that were created and later closed (ADR-0012).
    #[instrument(skip(self), name = "ledger.list_subaccounts")]
    pub async fn list_subaccounts(
        &self,
        account: &AccountId,
    ) -> Result<Vec<AccountId>, LedgerError> {
        let base = account.id;
        let mut subs: Vec<AccountId> = self
            .store
            .list_accounts()
            .await?
            .into_iter()
            .filter(|a| a.id.id == base && !a.is_closed())
            .map(|a| a.id)
            .collect();
        subs.sort();
        Ok(subs)
    }
}
