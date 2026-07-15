//! Canonical binary serialization — the content-addressing contract.
//!
//! Everything that defines the hash preimage lives here in one place: the
//! [`ToBytes`] trait, the [`CANONICAL_VERSION`] byte, the big-endian write
//! helpers, and every `impl ToBytes`. [`EnvelopeId`](crate::EnvelopeId) and the
//! account snapshot hashes are the double-SHA256 of these bytes, so a change to
//! any impl below changes those ids. Keeping the trait, the version, and all the
//! impls together makes the preimage auditable at a glance: what bytes are
//! hashed, in what order, is visible without holding three windows open.
//!
//! Encoding rules: integers are big-endian, variable-length sequences are
//! prefixed with a `u32` length, and the top-level [`Envelope`](crate::Envelope)
//! and [`Account`](crate::Account) preimages begin with [`CANONICAL_VERSION`].

use crate::{
    Account, AccountFlags, AccountId, AccountPolicy, AccountSnapshotId, AssetId, BookId, Cent,
    Envelope, EnvelopeId, NewPosting, Posting, PostingId, Receipt,
};

/// Deterministic binary serialization. Every domain type can produce its
/// canonical byte representation.
pub trait ToBytes {
    /// Returns the canonical byte representation of this value.
    fn to_bytes(&self) -> Vec<u8>;
}

/// Version byte prepended to canonical serializations for forward compatibility.
/// Bumped to 2 when `Cent` moved to a fixed 16-byte canonical encoding (ADR-0011).
/// Bumped to 3 when `AccountId` gained a `subaccount` leg folded into its
/// canonical bytes (ADR-0012).
/// Bumped to 4 when the vestigial `UserData` fields were removed from the
/// `Envelope` and `Account` preimages.
pub const CANONICAL_VERSION: u8 = 4;

/// Append a `u16` in big-endian to `buf`.
pub fn write_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_be_bytes());
}

/// Append a `u32` in big-endian to `buf`.
pub fn write_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_be_bytes());
}

/// Append a `u64` in big-endian to `buf`.
pub fn write_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_be_bytes());
}

/// Append an `i64` in big-endian to `buf`.
pub fn write_i64(buf: &mut Vec<u8>, v: i64) {
    buf.extend_from_slice(&v.to_be_bytes());
}

/// Append a `u128` in big-endian to `buf`.
pub fn write_u128(buf: &mut Vec<u8>, v: u128) {
    buf.extend_from_slice(&v.to_be_bytes());
}

impl ToBytes for Cent {
    fn to_bytes(&self) -> Vec<u8> {
        self.to_canonical_bytes().to_vec()
    }
}

impl ToBytes for AccountId {
    fn to_bytes(&self) -> Vec<u8> {
        // Base id then subaccount, both big-endian, so the subaccount is folded
        // into every content hash (envelope ids, posting ids, snapshots).
        let mut buf = Vec::with_capacity(16);
        buf.extend_from_slice(&self.id.to_be_bytes());
        buf.extend_from_slice(&self.sub.to_be_bytes());
        buf
    }
}

impl ToBytes for AccountSnapshotId {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(48);
        buf.extend_from_slice(&self.account.to_bytes());
        buf.extend_from_slice(&self.snapshot_id);
        buf
    }
}

impl ToBytes for AssetId {
    fn to_bytes(&self) -> Vec<u8> {
        self.0.to_be_bytes().to_vec()
    }
}

impl ToBytes for EnvelopeId {
    fn to_bytes(&self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl ToBytes for PostingId {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(34);
        buf.extend_from_slice(&self.transfer.0);
        write_u16(&mut buf, self.index);
        buf
    }
}

impl ToBytes for AccountPolicy {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(9);
        match self {
            Self::NoOverdraft => buf.push(0),
            Self::CappedOverdraft { floor } => {
                buf.push(1);
                buf.extend(floor.to_bytes());
            }
            Self::UncappedOverdraft => buf.push(2),
            Self::SystemAccount => buf.push(3),
            Self::ExternalAccount => buf.push(4),
        }
        buf
    }
}

impl ToBytes for AccountFlags {
    fn to_bytes(&self) -> Vec<u8> {
        self.bits().to_be_bytes().to_vec()
    }
}

impl ToBytes for BookId {
    fn to_bytes(&self) -> Vec<u8> {
        self.0.to_be_bytes().to_vec()
    }
}

impl ToBytes for NewPosting {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend(self.owner.to_bytes());
        buf.extend_from_slice(&self.asset.0.to_be_bytes());
        buf.extend(self.value.to_bytes());
        match &self.payer {
            Some(p) => {
                buf.push(1);
                buf.extend(p.to_bytes());
            }
            None => buf.push(0),
        }
        buf
    }
}

impl ToBytes for Posting {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend(self.id.to_bytes());
        buf.extend(self.owner.to_bytes());
        buf.extend_from_slice(&self.asset.0.to_be_bytes());
        buf.extend(self.value.to_bytes());
        buf
    }
}

impl ToBytes for Envelope {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(CANONICAL_VERSION);

        write_u32(&mut buf, self.consumes.len() as u32);
        for pid in &self.consumes {
            buf.extend(pid.to_bytes());
        }

        write_u32(&mut buf, self.creates.len() as u32);
        for np in &self.creates {
            buf.extend(np.to_bytes());
        }

        write_u32(&mut buf, self.account_snapshots.len() as u32);
        for snap in &self.account_snapshots {
            buf.extend(snap.to_bytes());
        }

        buf.extend(self.book.to_bytes());

        write_u32(&mut buf, self.metadata.len() as u32);
        for (key, value) in &self.metadata {
            let key_bytes = key.as_bytes();
            write_u32(&mut buf, key_bytes.len() as u32);
            buf.extend_from_slice(key_bytes);
            write_u32(&mut buf, value.len() as u32);
            buf.extend_from_slice(value);
        }

        buf
    }
}

impl ToBytes for Account {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(CANONICAL_VERSION);
        buf.extend(self.id.to_bytes());
        write_u64(&mut buf, self.version);
        buf.extend(self.policy.to_bytes());
        buf.extend(self.flags.to_bytes());
        buf.extend(self.book.to_bytes());

        write_u32(&mut buf, self.metadata.len() as u32);
        for (key, value) in &self.metadata {
            let key_bytes = key.as_bytes();
            write_u32(&mut buf, key_bytes.len() as u32);
            buf.extend_from_slice(key_bytes);
            write_u32(&mut buf, value.len() as u32);
            buf.extend_from_slice(value);
        }

        buf
    }
}

impl ToBytes for Receipt {
    fn to_bytes(&self) -> Vec<u8> {
        self.transfer_id.0.to_vec()
    }
}
