//! Auto ID generator — snowflake-inspired i64 ID scheme.
//!
//! Inspired by Twitter's Snowflake ID format (2010), adapted for
//! single-process embedded use with CRC32-based disambiguation.
//!
//! Bit layout:
//! ```text
//! [0][  40 bits: ms timestamp  ][ 23 bits: CRC32(data) or counter ]
//!  ^sign (always 0 = positive)
//! ```
//!
//! - Bit 63: always 0 (keeps i64 positive)
//! - Bits 62–23: milliseconds since [`KUATIA_EPOCH_MS`] (40 bits ≈ 34.8 years)
//! - Bits 22–0: lower 23 bits of CRC32 of context data, or an internal
//!   counter that wraps on overflow when no data is provided.
//!
//! The millisecond field counts from a fixed recent epoch
//! ([`KUATIA_EPOCH_MS`] = 2026-01-01T00:00:00Z) rather than the Unix epoch, so
//! the 40-bit window gives ~34.8 years of range *going forward* (until ~2060)
//! instead of a window already partly elapsed since 1970. Collision resistance
//! within a millisecond comes from the CRC32 tail (for content-keyed ids).

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const TIMESTAMP_BITS: u32 = 40;
const TAIL_BITS: u32 = 23;
const TAIL_MASK: u32 = (1 << TAIL_BITS) - 1;

/// Custom epoch for the timestamp field: 2026-01-01T00:00:00Z in Unix
/// milliseconds. Ids generated before this instant clamp to 0.
pub const KUATIA_EPOCH_MS: u64 = 1_767_225_600_000;

/// Snowflake-style ID generator.
///
/// Each generator holds an internal counter used when no CRC32 data is
/// provided. The counter wraps back to zero on overflow.
pub struct AutoId {
    counter: AtomicU32,
}

impl AutoId {
    /// Create a new generator with counter starting at zero.
    ///
    /// `const` so a single generator can back a process-global `static` (see
    /// `ReservationId::default`), giving ids that are unique across threads
    /// rather than per-thread.
    pub const fn new() -> Self {
        Self {
            counter: AtomicU32::new(0),
        }
    }

    /// Generate an ID using the lower 23 bits of CRC32 of `data`.
    pub fn next_with_data(&self, data: &[u8]) -> i64 {
        let ms = Self::now_ms();
        let crc = crc32(data);
        Self::pack(ms, crc & TAIL_MASK)
    }

    /// Generate an ID using the internal auto-incrementing counter.
    /// The counter wraps to zero on overflow of the 23-bit range.
    pub fn next(&self) -> i64 {
        let ms = Self::now_ms();
        let seq = self.counter.fetch_add(1, Ordering::Relaxed) & TAIL_MASK;
        Self::pack(ms, seq)
    }

    /// Extract the millisecond timestamp from an ID.
    pub fn timestamp(id: i64) -> u64 {
        ((id >> TAIL_BITS) & ((1i64 << TIMESTAMP_BITS) - 1)) as u64
    }

    /// Extract the tail (CRC32 or counter) from an ID.
    pub fn tail(id: i64) -> u32 {
        (id as u32) & TAIL_MASK
    }

    fn now_ms() -> u64 {
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        unix_ms.saturating_sub(KUATIA_EPOCH_MS)
    }

    fn pack(ms: u64, tail: u32) -> i64 {
        let ts = (ms & ((1u64 << TIMESTAMP_BITS) - 1)) as i64;
        (ts << TAIL_BITS) | (tail as i64)
    }
}

impl Default for AutoId {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// CRC32 (IEEE / ISO 3309)
// ---------------------------------------------------------------------------

const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut crc = i;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i as usize] = crc;
        i += 1;
    }
    table
};

/// Compute CRC32 (IEEE) of `data`.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        let idx = ((crc ^ byte as u32) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32_TABLE[idx];
    }
    crc ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_always_positive() {
        let sf = AutoId::new();
        for _ in 0..1000 {
            assert!(sf.next() > 0);
        }
    }

    #[test]
    fn id_with_data_is_positive() {
        let sf = AutoId::new();
        assert!(sf.next_with_data(b"hello") > 0);
        assert!(sf.next_with_data(b"") > 0);
    }

    #[test]
    fn timestamp_round_trips() {
        let sf = AutoId::new();
        let mask = (1u64 << 40) - 1;
        let before = AutoId::now_ms() & mask;
        let id = sf.next();
        let after = AutoId::now_ms() & mask;
        let ts = AutoId::timestamp(id);
        assert!(ts >= before && ts <= after);
    }

    #[test]
    fn different_data_different_tails() {
        let sf = AutoId::new();
        let a = sf.next_with_data(b"alice");
        let b = sf.next_with_data(b"bob");
        assert_ne!(AutoId::tail(a), AutoId::tail(b));
    }

    #[test]
    fn counter_increments() {
        let sf = AutoId::new();
        let a = sf.next();
        let b = sf.next();
        // Tails should differ by 1 (unless ms boundary crossed, but very unlikely)
        let ta = AutoId::tail(a);
        let tb = AutoId::tail(b);
        assert_eq!(tb, ta + 1);
    }

    #[test]
    fn counter_wraps() {
        let sf = AutoId::new();
        // Set counter just below the mask to test wrap
        sf.counter.store(TAIL_MASK, Ordering::Relaxed);
        let id = sf.next();
        assert_eq!(AutoId::tail(id), TAIL_MASK);
        let id2 = sf.next();
        assert_eq!(AutoId::tail(id2), 0);
    }

    #[test]
    fn crc32_known_vector() {
        // CRC32 of "123456789" is 0xCBF43926
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
