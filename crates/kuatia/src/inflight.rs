//! Inflight holds: authorize funds now, confirm (fully or partially) or void
//! later.
//!
//! An inflight transaction is an ordinary trade whose every destination is
//! rewritten to a per-destination holding subaccount (`NoOverdraft`, flagged
//! [`AccountFlags::INFLIGHT`], keyed by a subaccount derived from the trade).
//! Committing that rewritten transfer parks the
//! funds. Confirm and void are ordinary commits that move a hold's balance to
//! its destination or back to its funder. Nothing new is stored: the authorize
//! transfer's metadata carries the leg table, and every artifact is tagged with
//! a CBOR-encoded `InflightMeta` entry so the lifecycle is read, not inferred.
//!
//! See `doc/adr/0014-inflight-holds-via-holding-accounts.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use kuatia_core::{
    Account, AccountFlags, AccountId, AccountPolicy, AssetId, BookId, Cent, EnvelopeId, Metadata,
    Receipt, SelectionError, Transfer, TransferBuilder, hash::double_sha256,
};
use kuatia_storage::error::StoreError;
use kuatia_storage::store::EnvelopeRecord;
use serde::{Deserialize, Serialize};

use crate::error::LedgerError;
use crate::ledger::Ledger;

/// Single metadata key holding the CBOR-encoded [`InflightMeta`] payload.
const K_INFLIGHT: &str = "inflight";

/// One leg of an inflight transaction: an amount of an asset funded by `funder`,
/// parked in `hold`, destined for `destination`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InflightLeg {
    /// Account the funds settle to on confirm.
    pub destination: AccountId,
    /// Per-destination holding account parking the funds.
    pub hold: AccountId,
    /// Account that funded this leg (the funds return here on void).
    pub funder: AccountId,
    /// Asset being held.
    pub asset: AssetId,
    /// Amount authorized for this leg.
    pub amount: Cent,
}

/// Result of [`Ledger::authorize`].
#[derive(Debug, Clone)]
pub struct Authorization {
    /// Handle for the inflight transaction: the authorize transfer's id.
    pub inflight: EnvelopeId,
    /// The legs, one per original movement.
    pub legs: Vec<InflightLeg>,
}

impl Authorization {
    /// Receipt of the authorize commit.
    pub fn receipt(&self) -> Receipt {
        Receipt {
            transfer_id: self.inflight,
        }
    }
}

/// Lifecycle state of an inflight transaction, derived from balances and the
/// settling transfers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InflightState {
    /// Nothing settled yet; the full authorized amount is still held.
    Held,
    /// Some funds settled, some still held.
    PartiallyConfirmed,
    /// Fully settled to destinations.
    Confirmed,
    /// Fully returned to funders.
    Voided,
    /// Fully settled, but a mix of confirmed and voided legs.
    Mixed,
}

/// Per-(destination, asset) status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InflightLegStatus {
    /// Destination account.
    pub destination: AccountId,
    /// Holding account.
    pub hold: AccountId,
    /// Asset.
    pub asset: AssetId,
    /// Amount originally authorized.
    pub authorized: Cent,
    /// Amount confirmed to the destination so far.
    pub confirmed: Cent,
    /// Amount returned to funders so far.
    pub voided: Cent,
    /// Amount still held (`= authorized - confirmed - voided`).
    pub held: Cent,
}

/// Derived status of an inflight transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InflightStatus {
    /// The inflight handle.
    pub inflight: EnvelopeId,
    /// One entry per (destination, asset).
    pub legs: Vec<InflightLegStatus>,
    /// Overall state.
    pub state: InflightState,
}

// ---------------------------------------------------------------------------
// Metadata: one CBOR-encoded tagged payload under the `inflight` key
// ---------------------------------------------------------------------------

/// The inflight payload carried in a transfer's or holding account's metadata.
/// Serialized to CBOR (via `ciborium`) and stored under [`K_INFLIGHT`], so the
/// whole lifecycle is self-describing and read back, not inferred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum InflightMeta {
    /// Tags the authorize transfer and carries its leg table.
    Authorize { legs: Vec<InflightLeg> },
    /// Tags a per-destination holding subaccount.
    Hold { destination: AccountId },
    /// Tags a settling transfer that delivers to a destination.
    Confirm {
        tx: EnvelopeId,
        destination: AccountId,
    },
    /// Tags a settling transfer that returns to a funder.
    Void {
        tx: EnvelopeId,
        destination: AccountId,
    },
}

/// Whether a settle delivers to the destination or returns to a funder.
#[derive(Clone, Copy)]
enum SettleRole {
    Confirm,
    Void,
}

fn malformed(tid: EnvelopeId) -> LedgerError {
    LedgerError::NotInflightTransaction(tid)
}

/// Encode an [`InflightMeta`] to CBOR bytes.
fn encode_meta(meta: &InflightMeta) -> Result<Vec<u8>, LedgerError> {
    let mut buf = Vec::new();
    ciborium::into_writer(meta, &mut buf)
        .map_err(|e| LedgerError::Store(StoreError::Internal(e.to_string())))?;
    Ok(buf)
}

/// Wrap a single [`InflightMeta`] into a fresh [`Metadata`] map.
fn meta_map(meta: &InflightMeta) -> Result<Metadata, LedgerError> {
    let mut m = Metadata::new();
    m.insert(K_INFLIGHT.to_string(), encode_meta(meta)?);
    Ok(m)
}

/// Decode the [`InflightMeta`] carried by a metadata map, if any.
fn read_meta(meta: &Metadata) -> Option<InflightMeta> {
    let bytes = meta.get(K_INFLIGHT)?;
    ciborium::from_reader(bytes.as_slice()).ok()
}

impl Ledger {
    // -----------------------------------------------------------------------
    // Authorize
    // -----------------------------------------------------------------------

    /// Authorize a trade without settling it. Each movement's destination is
    /// rewritten to a fresh per-destination holding account, and the rewritten
    /// transfer is committed, parking the funds. Returns a handle used by
    /// [`confirm_all`](Self::confirm_all), [`confirm`](Self::confirm), and
    /// [`void`](Self::void).
    ///
    /// Every movement must move between two distinct accounts. All holds share a
    /// subaccount derived from the trade, so re-authorizing the identical trade is
    /// rejected (its holds already exist), while different trades to the same
    /// destination run concurrently under distinct subaccounts.
    pub async fn authorize(
        self: &Arc<Self>,
        transfer: Transfer,
    ) -> Result<Authorization, LedgerError> {
        // All holds of this inflight share one subaccount, derived from the
        // submitted trade so it is stable and known before the holds are created
        // (the authorize transfer's own id cannot be used: it is a hash of the
        // envelope that pays into the holds).
        let sub = inflight_subaccount(&transfer);

        // One holding subaccount per distinct destination: (destination, sub).
        let mut dest_to_hold: BTreeMap<AccountId, AccountId> = BTreeMap::new();
        for m in &transfer.movements {
            if m.from == m.to {
                return Err(LedgerError::InflightSelfMovement(m.from));
            }
            dest_to_hold
                .entry(m.to)
                .or_insert_with(|| AccountId::with_sub(m.to.id, sub));
        }

        // Create the holds. An existing (destination, sub) entity means this exact
        // trade is already inflight, so different trades (different subs) can hold
        // against the same destination at once.
        for (dest, hold) in &dest_to_hold {
            let mut acct = Account::new_ref(*hold, AccountPolicy::NoOverdraft);
            acct.flags = AccountFlags::INFLIGHT;
            acct.book = transfer.book;
            acct.metadata = meta_map(&InflightMeta::Hold { destination: *dest })?;
            match self.create_account(acct).await {
                Ok(()) => {}
                Err(LedgerError::Store(StoreError::AlreadyExists(_))) => {
                    return Err(LedgerError::InflightAlreadyOpen(*hold));
                }
                Err(e) => return Err(e),
            }
        }

        // Rewrite each movement funder -> hold and record the leg table.
        let mut legs = Vec::with_capacity(transfer.movements.len());
        let mut builder = TransferBuilder::new().book(transfer.book);
        for m in &transfer.movements {
            let hold = dest_to_hold[&m.to];
            legs.push(InflightLeg {
                destination: m.to,
                hold,
                funder: m.from,
                asset: m.asset,
                amount: m.amount,
            });
            builder = builder.movement_ref(m.from, hold, m.asset, m.amount);
        }
        let mut md = transfer.metadata.clone();
        md.insert(
            K_INFLIGHT.to_string(),
            encode_meta(&InflightMeta::Authorize { legs: legs.clone() })?,
        );
        let rewritten = builder.metadata(md).build();

        let receipt = self.commit(rewritten).await?;
        Ok(Authorization {
            inflight: receipt.transfer_id,
            legs,
        })
    }

    // -----------------------------------------------------------------------
    // Confirm
    // -----------------------------------------------------------------------

    /// Confirm the entire inflight transaction: sweep every hold's remaining
    /// balance to its destination and close the drained holds.
    pub async fn confirm_all(
        self: &Arc<Self>,
        inflight: &EnvelopeId,
    ) -> Result<Vec<Receipt>, LedgerError> {
        let (record, legs) = self.load_inflight(inflight).await?;
        let book = record.envelope.book();
        let mut receipts = Vec::new();
        for group in group_holds(&legs, *inflight)? {
            for asset in group.assets {
                let bal = self.balance(&group.hold, &asset).await?;
                if bal.is_positive() {
                    receipts.push(
                        self.settle(
                            book,
                            *inflight,
                            group.hold,
                            group.destination,
                            group.destination,
                            asset,
                            bal,
                            SettleRole::Confirm,
                        )
                        .await?,
                    );
                }
            }
            self.close_if_drained(&group.hold).await?;
        }
        Ok(receipts)
    }

    /// Confirm one or more legs in a single call. Each movement is expressed with
    /// the same `(from, to, asset, amount)` shape as [`TransferBuilder::pay`]:
    /// `from` is the leg's funder, `to` its destination. Build the set with
    /// `TransferBuilder` and pass the resulting [`Transfer`]; its book, user data,
    /// and metadata are ignored.
    ///
    /// Each movement delivers `amount` of `asset` from the matching leg's hold to
    /// its destination. `amount` must not exceed the amount still held; the
    /// `NoOverdraft` hold makes over-confirmation impossible regardless. A hold is
    /// closed once fully drained.
    ///
    /// Movements settle in order, each its own commit, so the batch is not atomic:
    /// a later movement failing leaves earlier confirmations applied.
    pub async fn confirm(
        self: &Arc<Self>,
        inflight: &EnvelopeId,
        confirms: Transfer,
    ) -> Result<Vec<Receipt>, LedgerError> {
        let (record, legs) = self.load_inflight(inflight).await?;
        let book = record.envelope.book();
        let mut receipts = Vec::new();
        let mut touched: BTreeSet<AccountId> = BTreeSet::new();
        for m in &confirms.movements {
            let leg = legs
                .iter()
                .find(|l| l.funder == m.from && l.destination == m.to && l.asset == m.asset)
                .ok_or(LedgerError::InflightLegNotFound {
                    destination: m.to,
                    asset: m.asset,
                })?;
            let held = self.balance(&leg.hold, &m.asset).await?;
            if m.amount > held {
                return Err(LedgerError::Selection(SelectionError::InsufficientFunds {
                    available: held,
                    requested: m.amount,
                }));
            }
            receipts.push(
                self.settle(
                    book,
                    *inflight,
                    leg.hold,
                    m.to,
                    m.to,
                    m.asset,
                    m.amount,
                    SettleRole::Confirm,
                )
                .await?,
            );
            touched.insert(leg.hold);
        }
        for hold in touched {
            self.close_if_drained(&hold).await?;
        }
        Ok(receipts)
    }

    // -----------------------------------------------------------------------
    // Void
    // -----------------------------------------------------------------------

    /// Void the entire inflight transaction: return every hold's remaining
    /// balance to the funders recorded in the leg table and close the holds.
    pub async fn void(
        self: &Arc<Self>,
        inflight: &EnvelopeId,
    ) -> Result<Vec<Receipt>, LedgerError> {
        let (record, legs) = self.load_inflight(inflight).await?;
        let book = record.envelope.book();
        let mut receipts = Vec::new();
        for group in group_holds(&legs, *inflight)? {
            for asset in &group.assets {
                let mut remaining = self.balance(&group.hold, asset).await?;
                // Return to funders in leg order, each up to what it funded. For
                // the common single-funder-per-(hold, asset) case this returns the
                // whole remaining balance to that funder.
                let mut funders: Vec<(AccountId, Cent)> = legs
                    .iter()
                    .filter(|l| l.hold == group.hold && l.asset == *asset)
                    .map(|l| (l.funder, l.amount))
                    .collect();
                // Ensure any co-funding rounding leftover lands on the last funder.
                if let Some(last) = funders.last_mut() {
                    last.1 = Cent::from(i64::MAX);
                }
                for (funder, cap) in funders {
                    if !remaining.is_positive() {
                        break;
                    }
                    let give = if cap < remaining { cap } else { remaining };
                    if give.is_positive() {
                        receipts.push(
                            self.settle(
                                book,
                                *inflight,
                                group.hold,
                                funder,
                                group.destination,
                                *asset,
                                give,
                                SettleRole::Void,
                            )
                            .await?,
                        );
                        remaining = remaining.checked_sub(give)?;
                    }
                }
            }
            self.close_if_drained(&group.hold).await?;
        }
        Ok(receipts)
    }

    // -----------------------------------------------------------------------
    // Status / queries
    // -----------------------------------------------------------------------

    /// Derived status of an inflight transaction: per-leg authorized, confirmed,
    /// voided, and still-held amounts, plus an overall state. All figures come
    /// from balances and the metadata-tagged settling transfers.
    pub async fn inflight_status(
        &self,
        inflight: &EnvelopeId,
    ) -> Result<InflightStatus, LedgerError> {
        let (_record, legs) = self.load_inflight(inflight).await?;
        let groups = group_holds(&legs, *inflight)?;

        // Authorized per (hold, asset).
        let mut authorized: BTreeMap<(AccountId, AssetId), Cent> = BTreeMap::new();
        for l in &legs {
            let e = authorized.entry((l.hold, l.asset)).or_insert(Cent::ZERO);
            *e = e.checked_add(l.amount)?;
        }

        // Confirmed / voided per (hold, asset), summed from settle transfers.
        let mut confirmed: BTreeMap<(AccountId, AssetId), Cent> = BTreeMap::new();
        let mut voided: BTreeMap<(AccountId, AssetId), Cent> = BTreeMap::new();
        for group in &groups {
            for record in self.history(&group.hold).await? {
                let bucket = match read_meta(record.envelope.metadata()) {
                    Some(InflightMeta::Confirm { .. }) => &mut confirmed,
                    Some(InflightMeta::Void { .. }) => &mut voided,
                    _ => continue,
                };
                for np in record.envelope.creates() {
                    if np.owner == group.hold {
                        continue; // change returned to the hold, not settled out
                    }
                    let e = bucket.entry((group.hold, np.asset)).or_insert(Cent::ZERO);
                    *e = e.checked_add(np.value)?;
                }
            }
        }

        let mut lines = Vec::new();
        for group in &groups {
            for asset in &group.assets {
                let held = self.balance(&group.hold, asset).await?;
                lines.push(InflightLegStatus {
                    destination: group.destination,
                    hold: group.hold,
                    asset: *asset,
                    authorized: authorized
                        .get(&(group.hold, *asset))
                        .copied()
                        .unwrap_or(Cent::ZERO),
                    confirmed: confirmed
                        .get(&(group.hold, *asset))
                        .copied()
                        .unwrap_or(Cent::ZERO),
                    voided: voided
                        .get(&(group.hold, *asset))
                        .copied()
                        .unwrap_or(Cent::ZERO),
                    held,
                });
            }
        }

        let state = overall_state(&lines);
        Ok(InflightStatus {
            inflight: *inflight,
            legs: lines,
            state,
        })
    }

    /// List the holding accounts of every currently open inflight (an
    /// `INFLIGHT`-flagged account that is not closed).
    pub async fn list_open_inflights(&self) -> Result<Vec<AccountId>, LedgerError> {
        Ok(self
            .list_accounts()
            .await?
            .into_iter()
            .filter(|a| a.flags.contains(AccountFlags::INFLIGHT) && !a.is_closed())
            .map(|a| a.id)
            .collect())
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Load the authorize transfer and decode its leg table.
    async fn load_inflight(
        &self,
        inflight: &EnvelopeId,
    ) -> Result<(EnvelopeRecord, Vec<InflightLeg>), LedgerError> {
        let record = self
            .store()
            .get_transfer(inflight)
            .await?
            .ok_or(LedgerError::InflightNotFound(*inflight))?;
        let legs = match read_meta(record.envelope.metadata()) {
            Some(InflightMeta::Authorize { legs }) => legs,
            _ => return Err(LedgerError::NotInflightTransaction(*inflight)),
        };
        Ok((record, legs))
    }

    /// Commit a `hold -> target` settling transfer tagged with the inflight role.
    #[allow(clippy::too_many_arguments)]
    async fn settle(
        self: &Arc<Self>,
        book: BookId,
        inflight: EnvelopeId,
        hold: AccountId,
        target: AccountId,
        destination: AccountId,
        asset: AssetId,
        amount: Cent,
        role: SettleRole,
    ) -> Result<Receipt, LedgerError> {
        let meta = match role {
            SettleRole::Confirm => InflightMeta::Confirm {
                tx: inflight,
                destination,
            },
            SettleRole::Void => InflightMeta::Void {
                tx: inflight,
                destination,
            },
        };
        let tx = TransferBuilder::new()
            .book(book)
            .pay_ref(hold, target, asset, amount)
            .metadata(meta_map(&meta)?)
            .build();
        self.commit(tx).await
    }

    /// Close a holding account once it has no live (active or reserved) postings
    /// left. No-op if already closed or still holding funds.
    async fn close_if_drained(&self, hold: &AccountId) -> Result<(), LedgerError> {
        if self.has_live_postings(hold).await? {
            return Ok(());
        }
        if !self.get_account(hold).await?.is_closed() {
            self.close(hold).await?;
        }
        Ok(())
    }
}

/// Derive the shared subaccount id for an inflight from the submitted trade.
/// Deterministic and known before the holds are created (unlike the authorize
/// transfer's id). The sign bit is masked so the id is always positive.
fn inflight_subaccount(transfer: &Transfer) -> i64 {
    let mut buf = Vec::new();
    // Serialization of a fixed `Transfer` is deterministic; the encode cannot
    // fail for these types, but on any error fall back to the empty buffer so
    // the result stays deterministic rather than panicking.
    let _ = ciborium::into_writer(transfer, &mut buf);
    let hash = double_sha256(&buf);
    let mut first = [0u8; 8];
    first.copy_from_slice(&hash[..8]);
    // Keep only the low SUB_BITS so the hold's subaccount id fits the account
    // code's encodable range (ADR-0015). The result is always positive.
    let mask = (1u64 << kuatia_types::SUB_BITS) - 1;
    (u64::from_be_bytes(first) & mask) as i64
}

/// A holding subaccount of an inflight together with its destination and the
/// assets it carries. Groups a leg table by hold so the confirm, void, and
/// status paths share one traversal instead of each re-deriving `holds_of` /
/// `destination_of` / `assets_of` inline.
struct HoldGroup {
    hold: AccountId,
    destination: AccountId,
    assets: Vec<AssetId>,
}

/// Group `legs` by holding subaccount, resolving each hold's destination. This
/// is the single "walk the holds of an inflight" traversal; it is pure over the
/// leg table and yields holds in sorted order (each with its assets sorted).
fn group_holds(legs: &[InflightLeg], inflight: EnvelopeId) -> Result<Vec<HoldGroup>, LedgerError> {
    holds_of(legs)
        .into_iter()
        .map(|hold| {
            Ok(HoldGroup {
                hold,
                destination: destination_of(legs, hold, inflight)?,
                assets: assets_of(legs, hold).into_iter().collect(),
            })
        })
        .collect()
}

fn holds_of(legs: &[InflightLeg]) -> BTreeSet<AccountId> {
    legs.iter().map(|l| l.hold).collect()
}

fn assets_of(legs: &[InflightLeg], hold: AccountId) -> BTreeSet<AssetId> {
    legs.iter()
        .filter(|l| l.hold == hold)
        .map(|l| l.asset)
        .collect()
}

fn destination_of(
    legs: &[InflightLeg],
    hold: AccountId,
    inflight: EnvelopeId,
) -> Result<AccountId, LedgerError> {
    legs.iter()
        .find(|l| l.hold == hold)
        .map(|l| l.destination)
        .ok_or_else(|| malformed(inflight))
}

fn overall_state(lines: &[InflightLegStatus]) -> InflightState {
    let mut any_held = false;
    let mut any_confirmed = false;
    let mut any_voided = false;
    for l in lines {
        if l.held.is_positive() {
            any_held = true;
        }
        if l.confirmed.is_positive() {
            any_confirmed = true;
        }
        if l.voided.is_positive() {
            any_voided = true;
        }
    }
    match (any_held, any_confirmed, any_voided) {
        (true, false, false) => InflightState::Held,
        (true, _, _) => InflightState::PartiallyConfirmed,
        (false, true, true) => InflightState::Mixed,
        (false, false, true) => InflightState::Voided,
        // Fully settled to destinations, or an empty/zero authorization.
        (false, _, false) => InflightState::Confirmed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuatia_core::AccountId;

    fn pay_trade(amount: i64) -> Transfer {
        TransferBuilder::new()
            .pay(
                AccountId::new(1),
                AccountId::new(2),
                AssetId::new(1),
                Cent::from(amount),
            )
            .build()
    }

    #[test]
    fn inflight_subaccount_is_deterministic_and_trade_specific() {
        let t1 = pay_trade(10);
        // Same trade -> same subaccount, so re-authorizing collides with itself.
        assert_eq!(inflight_subaccount(&t1), inflight_subaccount(&t1));
        // A different trade -> a different subaccount, so it can run concurrently.
        assert_ne!(
            inflight_subaccount(&t1),
            inflight_subaccount(&pay_trade(20))
        );
        // Never the main subaccount.
        assert_ne!(inflight_subaccount(&t1), 0);
    }
}
