//! Hashing and tamper-evidence for the ledger.
//!
//! Every transfer gets a content-addressed [`EnvelopeId`] (double-SHA256 of its
//! canonical serialization), which serves as both the idempotency key and the
//! tamper-evidence artifact.

use sha2::{Digest, Sha256};

use kuatia_types::{Account, AccountSnapshotId, Envelope, EnvelopeId, ToBytes};

// ---------------------------------------------------------------------------
// Double-SHA256
// ---------------------------------------------------------------------------

/// Double-SHA256 — the standard hash used throughout the ledger.
/// Prevents length-extension attacks.
pub fn double_sha256(data: &[u8]) -> [u8; 32] {
    let first = Sha256::digest(data);
    let second = Sha256::digest(first);
    let mut out = [0u8; 32];
    out.copy_from_slice(&second);
    out
}

// ---------------------------------------------------------------------------
// Transfer hashing
// ---------------------------------------------------------------------------

/// Deterministic binary serialization of an envelope.
pub fn canonical_bytes(envelope: &Envelope) -> Vec<u8> {
    envelope.to_bytes()
}

/// Double-SHA256 content hash. Returns a [`EnvelopeId`].
pub fn content_hash(data: &[u8]) -> EnvelopeId {
    EnvelopeId(double_sha256(data))
}

/// Convenience: `envelope.to_bytes()` → double-SHA256 → [`EnvelopeId`].
pub fn envelope_id(envelope: &Envelope) -> EnvelopeId {
    content_hash(&envelope.to_bytes())
}

// ---------------------------------------------------------------------------
// Account hashing
// ---------------------------------------------------------------------------

/// Deterministic binary serialization of an account snapshot.
pub fn account_canonical_bytes(account: &Account) -> Vec<u8> {
    account.to_bytes()
}

/// Double-SHA256 of an account's canonical bytes.
pub fn account_hash(account: &Account) -> [u8; 32] {
    double_sha256(&account.to_bytes())
}

/// Compute the [`AccountSnapshotId`] for an account's current state.
pub fn account_snapshot_id(account: &Account) -> AccountSnapshotId {
    AccountSnapshotId {
        account: account.id,
        snapshot_id: account_hash(account),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuatia_types::*;

    fn sample_envelope() -> Envelope {
        EnvelopeBuilder::new()
            .creates(vec![NewPosting {
                owner: AccountId::new(1),
                asset: AssetId::new(1),
                value: Cent::from(100),
                payer: None,
            }])
            .build()
    }

    #[test]
    fn content_hash_deterministic() {
        let t = sample_envelope();
        let id1 = envelope_id(&t);
        let id2 = envelope_id(&t);
        assert_eq!(id1, id2);
    }

    #[test]
    fn different_envelopes_different_hashes() {
        let t1 = sample_envelope();
        let mut t2 = sample_envelope();
        t2.creates[0].value = Cent::from(200);
        assert_ne!(envelope_id(&t1), envelope_id(&t2));
    }

    #[test]
    fn to_bytes_sha256_consistency() {
        let t = sample_envelope();
        assert_eq!(double_sha256(&t.to_bytes()), envelope_id(&t).0);
    }
}
