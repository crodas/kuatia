//! Shared row mappers and codecs: how the store turns domain values into the
//! text columns it stores, and rows back into domain types.
//!
//! Every column is a text type, so the database holds no opaque binary and a row
//! is legible in any SQL client. Content-addressed ids and opaque saga bytes are
//! stored as hex `TEXT`, JSON payloads as their `TEXT` serialization.

use std::str::FromStr;

use sqlx::Row;
use sqlx::any::AnyRow;

use kuatia_storage::error::StoreError;
use kuatia_types::*;

/// Serialize a value to a JSON string. Payload columns store JSON as `TEXT` so
/// the database is directly readable for auditing; the ledger never queries into
/// the JSON, so no binary or indexed representation is needed.
pub(crate) fn serialize_json<T: serde::Serialize>(val: &T) -> Result<String, StoreError> {
    serde_json::to_string(val).map_err(|e| StoreError::Internal(format!("json serialization: {e}")))
}

pub(crate) fn deserialize_json<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, StoreError> {
    serde_json::from_str(s).map_err(|e| StoreError::Internal(format!("bad json: {e}")))
}

/// Lower-case hex encoding. Binary identifiers (content-addressed hashes) and
/// opaque saga bytes are stored as hex `TEXT` so a row is legible in any SQL
/// client and matches the hex form used in logs and `Debug` output.
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

pub(crate) fn from_hex(s: &str) -> Result<Vec<u8>, StoreError> {
    if s.len() % 2 != 0 {
        return Err(StoreError::Internal(format!("odd-length hex: {s:?}")));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|e| StoreError::Internal(format!("bad hex: {e}")))
        })
        .collect()
}

pub(crate) fn envelope_id_to_hex(id: &EnvelopeId) -> String {
    to_hex(&id.0)
}

pub(crate) fn envelope_id_from_hex(s: &str) -> Result<EnvelopeId, StoreError> {
    let bytes = from_hex(s)?;
    let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        StoreError::Internal(format!("expected 32-byte id, got {} bytes", bytes.len()))
    })?;
    Ok(EnvelopeId(arr))
}

pub(crate) fn row_to_account(row: &AnyRow) -> Result<Account, StoreError> {
    let id: i64 = row
        .try_get("id")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let subaccount: i64 = row
        .try_get("subaccount")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let version: i64 = row
        .try_get("version")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let flags_bits: i32 = row
        .try_get("flags")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let book: i64 = row
        .try_get("book")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let metadata_json: String = row
        .try_get("metadata")
        .map_err(|e| StoreError::Internal(e.to_string()))?;

    Ok(Account {
        id: AccountId::with_sub(id, subaccount),
        version: version as u64,
        flags: AccountFlags::from_bits_truncate(flags_bits as u32),
        book: BookId::new(book),
        metadata: deserialize_json(&metadata_json)?,
    })
}

pub(crate) fn row_to_posting(row: &AnyRow) -> Result<Posting, StoreError> {
    let transfer_id: String = row
        .try_get("transfer_id")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let idx: i16 = row
        .try_get("idx")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let owner: i64 = row
        .try_get("owner")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let subaccount: i64 = row
        .try_get("subaccount")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let asset: i32 = row
        .try_get("asset")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let value: String = row
        .try_get("value")
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let value = Cent::from_str(&value).map_err(|e| StoreError::Internal(e.to_string()))?;

    Ok(Posting {
        id: PostingId {
            transfer: envelope_id_from_hex(&transfer_id)?,
            index: idx as u16,
        },
        owner: AccountId::with_sub(owner, subaccount),
        asset: AssetId::new(asset as u32),
        value,
    })
}
