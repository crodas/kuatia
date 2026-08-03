//! Pure inflight projection: the `InflightMeta` schema and the derivation rules
//! that turn a leg table, the settling transfers, and held balances into an
//! [`InflightStatus`].
//!
//! This is the single owner of the encode/decode halves so they cannot drift,
//! and of the void funder-distribution arithmetic. Everything here is pure (no
//! `self`, no IO, no async); the async [`Ledger`](crate::ledger::Ledger)
//! methods in the parent module only load raw records and call these functions.
//! It stays in the `kuatia` crate rather than moving to `kuatia-core` because it
//! reads [`EnvelopeRecord`], a `kuatia-storage` type the pure core avoids.

use std::collections::{BTreeMap, BTreeSet};

use kuatia_core::{AccountId, AssetId, Cent, EnvelopeId, Metadata, OverflowError};
use kuatia_storage::error::StoreError;
use kuatia_storage::store::EnvelopeRecord;
use serde::{Deserialize, Serialize};

use super::{InflightLeg, InflightLegStatus, InflightState, InflightStatus};
use crate::error::LedgerError;

/// Single metadata key holding the CBOR-encoded [`InflightMeta`] payload.
pub(super) const K_INFLIGHT: &str = "inflight";

// ---------------------------------------------------------------------------
// Metadata: one CBOR-encoded tagged payload under the `inflight` key
// ---------------------------------------------------------------------------

/// The inflight payload carried in a transfer's or holding account's metadata.
/// Serialized to CBOR (via `ciborium`) and stored under [`K_INFLIGHT`], so the
/// whole lifecycle is self-describing and read back, not inferred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum InflightMeta {
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

fn malformed(tid: EnvelopeId) -> LedgerError {
    LedgerError::NotInflightTransaction(tid)
}

/// Encode an [`InflightMeta`] to CBOR bytes.
pub(super) fn encode_meta(meta: &InflightMeta) -> Result<Vec<u8>, LedgerError> {
    let mut buf = Vec::new();
    ciborium::into_writer(meta, &mut buf)
        .map_err(|e| LedgerError::Store(StoreError::Internal(e.to_string())))?;
    Ok(buf)
}

/// Wrap a single [`InflightMeta`] into a fresh [`Metadata`] map.
pub(super) fn meta_map(meta: &InflightMeta) -> Result<Metadata, LedgerError> {
    let mut m = Metadata::new();
    m.insert(K_INFLIGHT.to_string(), encode_meta(meta)?);
    Ok(m)
}

/// Decode the [`InflightMeta`] carried by a metadata map, if any. Absent or
/// malformed metadata yields `None` rather than an error.
pub(super) fn read_meta(meta: &Metadata) -> Option<InflightMeta> {
    let bytes = meta.get(K_INFLIGHT)?;
    ciborium::from_reader(bytes.as_slice()).ok()
}

// ---------------------------------------------------------------------------
// Hold grouping: the single "walk the holds of an inflight" traversal
// ---------------------------------------------------------------------------

/// A holding subaccount of an inflight together with its destination and the
/// assets it carries. Groups a leg table by hold so the confirm, void, and
/// status paths share one traversal instead of each re-deriving `holds_of` /
/// `destination_of` / `assets_of` inline.
pub(super) struct HoldGroup {
    pub(super) hold: AccountId,
    pub(super) destination: AccountId,
    pub(super) assets: Vec<AssetId>,
}

/// Group `legs` by holding subaccount, resolving each hold's destination. This
/// is the single "walk the holds of an inflight" traversal; it is pure over the
/// leg table and yields holds in sorted order (each with its assets sorted).
pub(super) fn group_holds(
    legs: &[InflightLeg],
    inflight: EnvelopeId,
) -> Result<Vec<HoldGroup>, LedgerError> {
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

// ---------------------------------------------------------------------------
// Status derivation
// ---------------------------------------------------------------------------

/// Pre-loaded state for [`derive_status`], mirroring `PlanInput`: the async
/// layer fetches the raw records and balances, this struct names exactly what
/// the projection reads.
pub(super) struct StatusInput<'a> {
    /// The inflight handle.
    pub(super) inflight: EnvelopeId,
    /// The leg table from the authorize transfer.
    pub(super) legs: &'a [InflightLeg],
    /// Settle transfers found in each hold's history, tagged with that hold.
    pub(super) hold_history: &'a [(AccountId, Vec<EnvelopeRecord>)],
    /// Live held balance per (hold, asset).
    pub(super) held: &'a BTreeMap<(AccountId, AssetId), Cent>,
}

/// Fold the leg table, settling transfers, and held balances into an
/// [`InflightStatus`]. Pure: the caller supplies all state via [`StatusInput`].
///
/// Re-derives its own hold grouping via [`group_holds`] rather than trusting a
/// caller-supplied grouping, the same way `validate_and_plan` re-derives its
/// account sets; the loader's grouping is only used to decide what to fetch.
pub(super) fn derive_status(input: StatusInput<'_>) -> Result<InflightStatus, LedgerError> {
    let StatusInput {
        inflight,
        legs,
        hold_history,
        held,
    } = input;
    let groups = group_holds(legs, inflight)?;

    // Authorized per (hold, asset).
    let mut authorized: BTreeMap<(AccountId, AssetId), Cent> = BTreeMap::new();
    for l in legs {
        let e = authorized.entry((l.hold, l.asset)).or_insert(Cent::ZERO);
        *e = e.checked_add(l.amount)?;
    }

    // Index history by hold so attribution keys on the fetched-for hold, never
    // on the records' order relative to `groups`.
    let history_by_hold: BTreeMap<AccountId, &[EnvelopeRecord]> = hold_history
        .iter()
        .map(|(hold, recs)| (*hold, recs.as_slice()))
        .collect();

    // Confirmed / voided per (hold, asset), summed from settle transfers.
    let mut confirmed: BTreeMap<(AccountId, AssetId), Cent> = BTreeMap::new();
    let mut voided: BTreeMap<(AccountId, AssetId), Cent> = BTreeMap::new();
    for group in &groups {
        let records = history_by_hold.get(&group.hold).copied().unwrap_or(&[][..]);
        for record in records {
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
                held: held
                    .get(&(group.hold, *asset))
                    .copied()
                    .unwrap_or(Cent::ZERO),
            });
        }
    }

    let state = overall_state(&lines);
    Ok(InflightStatus {
        inflight,
        legs: lines,
        state,
    })
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

// ---------------------------------------------------------------------------
// Void funder distribution
// ---------------------------------------------------------------------------

/// One funder's share of a voided hold balance.
pub(super) struct FunderPayout {
    pub(super) funder: AccountId,
    pub(super) give: Cent,
}

/// Split `remaining` back to the funders of `(hold, asset)` in leg order, each
/// capped at what it funded. Any co-funding rounding leftover lands on the last
/// funder (its cap is lifted). Returns only positive payouts; an empty funder
/// set yields no payouts. Pure arithmetic.
pub(super) fn distribute_to_funders(
    legs: &[InflightLeg],
    hold: AccountId,
    asset: AssetId,
    mut remaining: Cent,
) -> Result<Vec<FunderPayout>, OverflowError> {
    let mut funders: Vec<(AccountId, Cent)> = legs
        .iter()
        .filter(|l| l.hold == hold && l.asset == asset)
        .map(|l| (l.funder, l.amount))
        .collect();
    // Ensure any co-funding rounding leftover lands on the last funder.
    if let Some(last) = funders.last_mut() {
        last.1 = Cent::from(i64::MAX);
    }

    let mut payouts = Vec::new();
    for (funder, cap) in funders {
        if !remaining.is_positive() {
            break;
        }
        let give = if cap < remaining { cap } else { remaining };
        if give.is_positive() {
            payouts.push(FunderPayout { funder, give });
            remaining = remaining.checked_sub(give)?;
        }
    }
    Ok(payouts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuatia_core::{BookId, Envelope, NewPosting, Receipt};

    // -- overall_state golden vectors --------------------------------------

    fn line(authorized: i64, confirmed: i64, voided: i64, held: i64) -> InflightLegStatus {
        InflightLegStatus {
            destination: AccountId::new(2),
            hold: AccountId::with_sub(2, 7),
            asset: AssetId::new(1),
            authorized: Cent::from(authorized),
            confirmed: Cent::from(confirmed),
            voided: Cent::from(voided),
            held: Cent::from(held),
        }
    }

    #[test]
    fn overall_state_held() {
        assert_eq!(overall_state(&[line(100, 0, 0, 100)]), InflightState::Held);
    }

    #[test]
    fn overall_state_partially_confirmed() {
        // Some settled out, some still held.
        assert_eq!(
            overall_state(&[line(100, 40, 0, 60)]),
            InflightState::PartiallyConfirmed
        );
    }

    #[test]
    fn overall_state_confirmed() {
        assert_eq!(
            overall_state(&[line(100, 100, 0, 0)]),
            InflightState::Confirmed
        );
    }

    #[test]
    fn overall_state_voided() {
        assert_eq!(
            overall_state(&[line(100, 0, 100, 0)]),
            InflightState::Voided
        );
    }

    #[test]
    fn overall_state_mixed() {
        // Nothing held; one leg confirmed, another voided.
        assert_eq!(
            overall_state(&[line(100, 100, 0, 0), line(100, 0, 100, 0)]),
            InflightState::Mixed
        );
    }

    #[test]
    fn overall_state_empty_authorization_is_confirmed() {
        // No legs at all, and a zero-amount leg, both hit the
        // `(false, _, false)` catch-all. We keep this as Confirmed by design.
        assert_eq!(overall_state(&[]), InflightState::Confirmed);
        assert_eq!(overall_state(&[line(0, 0, 0, 0)]), InflightState::Confirmed);
    }

    // -- distribute_to_funders golden vectors ------------------------------

    fn leg(funder: i64, hold_sub: i64, asset: u32, amount: i64) -> InflightLeg {
        InflightLeg {
            destination: AccountId::new(2),
            hold: AccountId::with_sub(2, hold_sub),
            funder: AccountId::new(funder),
            asset: AssetId::new(asset),
            amount: Cent::from(amount),
        }
    }

    #[test]
    fn distribute_single_funder_gets_whole_balance() {
        let legs = [leg(1, 7, 1, 100)];
        let out = distribute_to_funders(
            &legs,
            AccountId::with_sub(2, 7),
            AssetId::new(1),
            Cent::from(80),
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].funder, AccountId::new(1));
        // Last (only) funder's cap is lifted, so the whole remaining is returned.
        assert_eq!(out[0].give, Cent::from(80));
    }

    #[test]
    fn distribute_co_funders_split_by_cap() {
        // Two funders of 60 and 40; a remaining 100 splits 60 / 40.
        let legs = [leg(1, 7, 1, 60), leg(3, 7, 1, 40)];
        let out = distribute_to_funders(
            &legs,
            AccountId::with_sub(2, 7),
            AssetId::new(1),
            Cent::from(100),
        )
        .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            (out[0].funder, out[0].give),
            (AccountId::new(1), Cent::from(60))
        );
        assert_eq!(
            (out[1].funder, out[1].give),
            (AccountId::new(3), Cent::from(40))
        );
    }

    #[test]
    fn distribute_rounding_leftover_lands_on_last_funder() {
        // First funder capped at 60; the last funder's cap is lifted so it
        // absorbs the leftover (100 - 60 = 40, even though it funded only 30).
        let legs = [leg(1, 7, 1, 60), leg(3, 7, 1, 30)];
        let out = distribute_to_funders(
            &legs,
            AccountId::with_sub(2, 7),
            AssetId::new(1),
            Cent::from(100),
        )
        .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].give, Cent::from(60));
        assert_eq!(out[1].give, Cent::from(40));
    }

    #[test]
    fn distribute_no_funders_yields_no_payouts() {
        let legs: [InflightLeg; 0] = [];
        let out = distribute_to_funders(
            &legs,
            AccountId::with_sub(2, 7),
            AssetId::new(1),
            Cent::from(100),
        )
        .unwrap();
        assert!(out.is_empty());
    }

    // -- derive_status end-to-end vectors ----------------------------------

    fn settle_record(meta: InflightMeta, creates: Vec<NewPosting>) -> EnvelopeRecord {
        EnvelopeRecord {
            envelope: Envelope {
                consumes: vec![],
                creates,
                account_snapshots: vec![],
                book: BookId(0),
                metadata: meta_map(&meta).unwrap(),
            },
            receipt: Receipt {
                transfer_id: EnvelopeId([0; 32]),
            },
            created_at: 0,
        }
    }

    fn np(owner: AccountId, asset: u32, value: i64) -> NewPosting {
        NewPosting {
            owner,
            asset: AssetId::new(asset),
            value: Cent::from(value),
            payer: None,
        }
    }

    #[test]
    fn derive_status_confirm_skips_change_to_hold() {
        // A single leg authorized 100. One confirm settle delivers 60 to the
        // destination and returns 40 change to the hold. The change posting must
        // not count toward `confirmed`.
        let inflight = EnvelopeId([9; 32]);
        let hold = AccountId::with_sub(2, 7);
        let dest = AccountId::new(2);
        let legs = [leg(1, 7, 1, 100)];

        let confirm = settle_record(
            InflightMeta::Confirm {
                tx: inflight,
                destination: dest,
            },
            vec![np(dest, 1, 60), np(hold, 1, 40)],
        );
        let hold_history = vec![(hold, vec![confirm])];
        let mut held = BTreeMap::new();
        held.insert((hold, AssetId::new(1)), Cent::from(40));

        let status = derive_status(StatusInput {
            inflight,
            legs: &legs,
            hold_history: &hold_history,
            held: &held,
        })
        .unwrap();

        assert_eq!(status.legs.len(), 1);
        let l = status.legs[0];
        assert_eq!(l.authorized, Cent::from(100));
        assert_eq!(l.confirmed, Cent::from(60)); // change to hold excluded
        assert_eq!(l.voided, Cent::ZERO);
        assert_eq!(l.held, Cent::from(40));
        assert_eq!(status.state, InflightState::PartiallyConfirmed);
    }

    #[test]
    fn derive_status_confirmed_and_voided_bucketing() {
        // Authorized 100, fully settled: 70 confirmed to the destination, 30
        // voided back to the funder, nothing held -> Mixed.
        let inflight = EnvelopeId([9; 32]);
        let hold = AccountId::with_sub(2, 7);
        let dest = AccountId::new(2);
        let funder = AccountId::new(1);
        let legs = [leg(1, 7, 1, 100)];

        let confirm = settle_record(
            InflightMeta::Confirm {
                tx: inflight,
                destination: dest,
            },
            vec![np(dest, 1, 70)],
        );
        let void = settle_record(
            InflightMeta::Void {
                tx: inflight,
                destination: dest,
            },
            vec![np(funder, 1, 30)],
        );
        let hold_history = vec![(hold, vec![confirm, void])];
        let mut held = BTreeMap::new();
        held.insert((hold, AssetId::new(1)), Cent::ZERO);

        let status = derive_status(StatusInput {
            inflight,
            legs: &legs,
            hold_history: &hold_history,
            held: &held,
        })
        .unwrap();

        let l = status.legs[0];
        assert_eq!(l.confirmed, Cent::from(70));
        assert_eq!(l.voided, Cent::from(30));
        assert_eq!(l.held, Cent::ZERO);
        assert_eq!(status.state, InflightState::Mixed);
    }

    #[test]
    fn derive_status_all_held_when_no_settles() {
        let inflight = EnvelopeId([9; 32]);
        let hold = AccountId::with_sub(2, 7);
        let legs = [leg(1, 7, 1, 100)];
        let hold_history = vec![(hold, vec![])];
        let mut held = BTreeMap::new();
        held.insert((hold, AssetId::new(1)), Cent::from(100));

        let status = derive_status(StatusInput {
            inflight,
            legs: &legs,
            hold_history: &hold_history,
            held: &held,
        })
        .unwrap();

        assert_eq!(status.legs[0].held, Cent::from(100));
        assert_eq!(status.state, InflightState::Held);
    }
}
