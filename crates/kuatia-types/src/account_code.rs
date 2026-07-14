//! IBAN-style string codec for [`AccountId`] (ADR-0012, ADR-0015).
//!
//! A deep module. The account code is a fixed 20-character string: an
//! 18-character base-36 body followed by two ISO 7064 mod-97 check digits, with
//! no country code. It is produced and consumed through a narrow interface of
//! three methods on [`AccountId`]: [`Display`](fmt::Display) (machine form),
//! [`FromStr`](std::str::FromStr) (validating parse), and
//! [`to_grouped`](AccountId::to_grouped) (presentation spacing).
//!
//! Everything behind that interface is private: bit-packing the `(id, sub)`
//! pair, the base-36 rendering, the ISO 7064 mod-97 check digits, and the keyed
//! Feistel permutation (with cycle-walking) that hides the raw ids. Only the
//! obfuscation seed controls ([`set_id_seed`], [`id_seed`], [`DEFAULT_ID_SEED`])
//! and the two bit-width constants ([`ID_BITS`], [`SUB_BITS`]) are exposed
//! alongside the interface, because callers need them to reason about the
//! encodable range and to key the deployment's codes.

use crate::AccountId;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Bit-packing
// ---------------------------------------------------------------------------

/// Bits of the base `id` the string form encodes. A snowflake id keeps the sign
/// bit clear, so its value fits in 63 bits.
pub const ID_BITS: u32 = 63;

/// Bits of the `sub` leg the string form encodes. Together with [`ID_BITS`] this
/// is the 93-bit packed value the code carries, which fits in the 18 base-36
/// characters of the code body. A subaccount id must fit in this many bits to
/// round-trip through the string form.
pub const SUB_BITS: u32 = 30;

/// Number of base-36 characters in the code body. `36^18 > 2^93`, so the packed
/// `(id, sub)` value always fits.
const BODY_LEN: usize = 18;

/// Mask selecting the low [`ID_BITS`] + [`SUB_BITS`] = 93 bits of the packed value.
const PACK_MASK: u128 = (1u128 << (ID_BITS + SUB_BITS)) - 1;

/// Pack `(id, sub)` into the 93-bit value the code encodes: the low [`ID_BITS`]
/// of `id` in the high part, the low [`SUB_BITS`] of `sub` in the low part.
fn pack(id: i64, sub: i64) -> u128 {
    let id = (id as u128) & ((1u128 << ID_BITS) - 1);
    let sub = (sub as u128) & ((1u128 << SUB_BITS) - 1);
    (id << SUB_BITS) | sub
}

/// Inverse of [`pack`]: split the 93-bit value back into the two legs. The `id`
/// part is at most 63 bits, so it is always a non-negative `i64`.
fn unpack(p: u128) -> (i64, i64) {
    let sub = (p & ((1u128 << SUB_BITS) - 1)) as i64;
    let id = (p >> SUB_BITS) as i64;
    (id, sub)
}

/// Encode a value `< 2^93` as exactly [`BODY_LEN`] base-36 digits (`0-9A-Z`),
/// zero-padded on the left. `36^18 > 2^93`, so this never truncates.
fn base36_body(mut v: u128) -> String {
    const D: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut out = [b'0'; BODY_LEN];
    let mut i = out.len();
    while v > 0 && i > 0 {
        i -= 1;
        out[i] = D[(v % 36) as usize];
        v /= 36;
    }
    out.iter().map(|&b| b as char).collect()
}

/// Expand an IBAN string to its numeric form for the checksum: digits stay,
/// letters `A-Z` become `10..35`.
fn iban_expand(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.bytes() {
        if c.is_ascii_digit() {
            out.push(c as char);
        } else {
            let v = (c - b'A') as u32 + 10;
            out.push_str(&v.to_string());
        }
    }
    out
}

/// ISO 7064 mod-97-10 over a decimal string, computed iteratively so the input
/// length is unbounded.
fn mod97(digits: &str) -> u32 {
    let mut rem = 0u32;
    for b in digits.bytes() {
        rem = (rem * 10 + (b - b'0') as u32) % 97;
    }
    rem
}

/// The two mod-97 check digits for a base-36 body, IBAN-style but with no
/// country code: `98 - (expand(body ++ "00") mod 97)`.
fn check_digits(body: &str) -> u32 {
    98 - mod97(&iban_expand(&format!("{body}00")))
}

// ---------------------------------------------------------------------------
// Account-code obfuscation (ADR-0012, ADR-0015)
//
// The account code's body is a base-36 rendering of the packed (id, sub) value.
// Without mixing, small ids render as long runs of zeros that reveal their value
// and sequence. To hide that from outsiders, the 93-bit packed value is run
// through a keyed format-preserving permutation before encoding, and inverted on
// parse. The permutation is a Feistel network over 2^94 restricted to the 93-bit
// domain by cycle-walking (re-encrypt while the result exceeds the domain), which
// keeps it a bijection on the exact set of packable values.
//
// This is obfuscation, not security: anyone with the seed can decode it, so it
// is not a substitute for authorization. The seed has a default and can be set
// once at startup via `set_id_seed`; changing it changes every code, so it must
// be stable across a deployment.
// ---------------------------------------------------------------------------

/// Default seed for the account-code obfuscation permutation. Override at
/// startup with [`set_id_seed`], before any code is issued or parsed.
pub const DEFAULT_ID_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// Process-global seed keying the account-code permutation.
static ID_SEED: AtomicU64 = AtomicU64::new(DEFAULT_ID_SEED);

/// Set the process-global seed that keys the account-code obfuscation. Call once
/// at startup: every [`AccountId`] string form depends on it, so changing it
/// after codes are issued invalidates the previously issued ones.
pub fn set_id_seed(seed: u64) {
    ID_SEED.store(seed, Ordering::Relaxed);
}

/// The current process-global account-code seed.
pub fn id_seed() -> u64 {
    ID_SEED.load(Ordering::Relaxed)
}

/// Number of Feistel rounds. Four rounds of a strong round function give a
/// strong pseudo-random permutation (Luby-Rackoff), which is ample for
/// obfuscation.
const FEISTEL_ROUNDS: usize = 4;

/// SplitMix64 finalizer: a strong 64-bit avalanche mixer.
fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Per-round subkey derived from the seed and round index.
fn round_key(seed: u64, round: usize) -> u64 {
    mix64(seed ^ (round as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

/// Bits per Feistel half. Two halves give a 94-bit block, the smallest even
/// width that covers the 93-bit packed domain (one bit of headroom that
/// cycle-walking absorbs).
const FEISTEL_HALF_BITS: u32 = 47;

/// Mask selecting one [`FEISTEL_HALF_BITS`]-wide half.
const FEISTEL_HALF_MASK: u128 = (1u128 << FEISTEL_HALF_BITS) - 1;

/// Keyed Feistel permutation over the 94-bit block `(l, r)` (two 47-bit halves).
/// A balanced Feistel with any round function is a bijection on the full 2^94
/// space, which is what [`obfuscate`] cycle-walks down to the 93-bit domain.
fn feistel94(block: u128, seed: u64) -> u128 {
    let mut l = (block >> FEISTEL_HALF_BITS) & FEISTEL_HALF_MASK;
    let mut r = block & FEISTEL_HALF_MASK;
    for round in 0..FEISTEL_ROUNDS {
        let f = (mix64(r as u64 ^ round_key(seed, round)) as u128) & FEISTEL_HALF_MASK;
        let next = (l ^ f) & FEISTEL_HALF_MASK;
        l = r;
        r = next;
    }
    (l << FEISTEL_HALF_BITS) | r
}

/// Inverse of [`feistel94`] under the same seed.
fn feistel94_inv(block: u128, seed: u64) -> u128 {
    let mut l = (block >> FEISTEL_HALF_BITS) & FEISTEL_HALF_MASK;
    let mut r = block & FEISTEL_HALF_MASK;
    for round in (0..FEISTEL_ROUNDS).rev() {
        let f = (mix64(l as u64 ^ round_key(seed, round)) as u128) & FEISTEL_HALF_MASK;
        let prev = (r ^ f) & FEISTEL_HALF_MASK;
        r = l;
        l = prev;
    }
    (l << FEISTEL_HALF_BITS) | r
}

/// Keyed format-preserving permutation over the 93-bit packed domain. Applies
/// [`feistel94`] and cycle-walks (re-encrypts while the result exceeds the
/// domain) so the output is always a valid packed value. Since the domain is
/// half of 2^94, this averages about two iterations and always terminates.
fn obfuscate(p: u128, seed: u64) -> u128 {
    let mut v = p;
    loop {
        v = feistel94(v, seed);
        if v <= PACK_MASK {
            return v;
        }
    }
}

/// Inverse of [`obfuscate`] under the same seed: cycle-walk [`feistel94_inv`].
fn deobfuscate(y: u128, seed: u64) -> u128 {
    let mut v = y;
    loop {
        v = feistel94_inv(v, seed);
        if v <= PACK_MASK {
            return v;
        }
    }
}

// ---------------------------------------------------------------------------
// The interface: Display / FromStr / to_grouped
// ---------------------------------------------------------------------------

impl fmt::Display for AccountId {
    /// IBAN-style machine format: a fixed 20 characters, an 18-character base-36
    /// body followed by two trailing ISO 7064 mod-97 check digits, with no
    /// country code. The `(id, sub)` pair is packed into a 93-bit value and run
    /// through a keyed format-preserving permutation (see [`set_id_seed`]) before
    /// encoding, so the body does not reveal the raw ids. Round-trips via
    /// [`FromStr`](std::str::FromStr); [`to_grouped`](AccountId::to_grouped) adds
    /// the presentation spacing (five groups of four).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let body = base36_body(obfuscate(pack(self.id, self.sub), id_seed()));
        write!(f, "{body}{:02}", check_digits(&body))
    }
}

impl AccountId {
    /// IBAN-style presentation format: the machine [`Display`](fmt::Display)
    /// form grouped into blocks of four with a single space. The code is a fixed
    /// 20 characters, so this is always five groups of four
    /// (e.g. `K3P9 WM2X Q7ND V8HT 2R47`).
    pub fn to_grouped(&self) -> String {
        let machine = self.to_string();
        let mut out = String::with_capacity(machine.len() + machine.len() / 4);
        for (i, c) in machine.chars().enumerate() {
            if i > 0 && i % 4 == 0 {
                out.push(' ');
            }
            out.push(c);
        }
        out
    }
}

/// Returned when a string is not a valid [`AccountId`] code: wrong structure,
/// non-base-36 body, or a failed mod-97 checksum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseAccountIdError;

impl fmt::Display for ParseAccountIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid AccountId: not a checksum-valid account code")
    }
}

impl std::error::Error for ParseAccountIdError {}

impl std::str::FromStr for AccountId {
    type Err = ParseAccountIdError;

    /// Parse an IBAN-style account code back into the two legs. Any spaces
    /// (grouped display format) and dashes (URL-safe separator) are ignored and
    /// the input is upper-cased first, so `K3P9...`, `K3P9 WM2X ...`, and
    /// `K3P9-WM2X-...` all parse to the same id. The value must reduce to an
    /// 18-character base-36 body followed by two check digits, the ISO 7064
    /// mod-97 checksum must pass, and the decoded body must lie in the packable
    /// domain — so a mistyped or otherwise invalid id is rejected here rather
    /// than reaching the store.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let cleaned: String = s
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .map(|c| c.to_ascii_uppercase())
            .collect();
        // 18-char base-36 body + 2 trailing check digits.
        if cleaned.len() != BODY_LEN + 2 {
            return Err(ParseAccountIdError);
        }
        let body = &cleaned[0..BODY_LEN];
        let check = &cleaned[BODY_LEN..BODY_LEN + 2];
        let is_base36 = |b: u8| b.is_ascii_digit() || b.is_ascii_uppercase();
        if !check.bytes().all(|b| b.is_ascii_digit()) || !body.bytes().all(is_base36) {
            return Err(ParseAccountIdError);
        }
        // Checksum-valid iff the expanded (body ++ check) reduces to 1 under
        // mod-97.
        if mod97(&iban_expand(&format!("{body}{check}"))) != 1 {
            return Err(ParseAccountIdError);
        }
        // Decode the body, reject anything outside the 93-bit packable domain
        // (the permutation's image), then invert the permutation and unpack.
        let obf = u128::from_str_radix(body, 36).map_err(|_| ParseAccountIdError)?;
        if obf > PACK_MASK {
            return Err(ParseAccountIdError);
        }
        let (id, sub) = unpack(deobfuscate(obf, id_seed()));
        Ok(Self { id, sub })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// Largest subaccount id that round-trips through the string form.
    const MAX_SUB: i64 = (1i64 << SUB_BITS) - 1;

    #[test]
    fn code_structure() {
        let s = AccountId::with_sub(5, 7).to_string();
        // Fixed 20 chars: an 18-char base-36 body then two check digits, no
        // country code.
        assert_eq!(s.len(), 20);
        assert_eq!(s.len(), BODY_LEN + 2);
        // The two trailing check digits are numeric.
        assert!(s[BODY_LEN..].bytes().all(|b| b.is_ascii_digit()));
        // The body is permuted, so it does NOT expose the raw legs the way an
        // unmixed base-36 rendering (a run of zeros then "5"/"7") would.
        assert_ne!(&s[..BODY_LEN], "000000000000000527");
    }

    #[test]
    fn code_round_trips() {
        for acc in [
            AccountId::new(0),
            AccountId::new(100),
            AccountId::with_sub(5, 7),
            AccountId::with_sub(987654321, 12345),
            // The corners of the encodable domain: a full 63-bit id and the
            // widest subaccount.
            AccountId::new(i64::MAX),
            AccountId::with_sub(i64::MAX, MAX_SUB),
            AccountId::with_sub(0, MAX_SUB),
        ] {
            let s = acc.to_string();
            assert_eq!(s.len(), 20, "length {s}");
            assert_eq!(AccountId::from_str(&s).unwrap(), acc, "round-trip {s}");
        }
    }

    #[test]
    fn out_of_range_legs_truncate_to_the_encodable_domain() {
        // The string form encodes only the low ID_BITS / SUB_BITS. A subaccount
        // one past the encodable range wraps to 0 (documented narrowing), so it
        // shares its code with the main account rather than corrupting it.
        let over = AccountId::with_sub(7, 1i64 << SUB_BITS);
        assert_eq!(
            AccountId::from_str(&over.to_string()).unwrap(),
            AccountId::new(7)
        );
    }

    #[test]
    fn parses_a_fixed_vector() {
        // A hardcoded, checksum-valid code (under DEFAULT_ID_SEED) pins the
        // exact encoding, permutation, and checksum, so an accidental change to
        // any of them is caught by a failing parse.
        let code = "KUJLQEL8IX2XGTBK4425";
        let expected = AccountId::with_sub(987654321, 12345);
        assert_eq!(AccountId::from_str(code).unwrap(), expected);
        // The grouped (spaced, lower-cased) form parses to the same value.
        assert_eq!(
            AccountId::from_str("kujl qel8 ix2x gtbk 4425").unwrap(),
            expected
        );
        // Display reproduces the exact machine form.
        assert_eq!(expected.to_string(), code);
    }

    #[test]
    fn obfuscation_permutation_is_invertible_across_seeds() {
        for &seed in &[0u64, 1, DEFAULT_ID_SEED, u64::MAX] {
            for &p in &[0u128, 1, 5 << SUB_BITS | 7, PACK_MASK, PACK_MASK - 1] {
                let y = obfuscate(p, seed);
                assert!(y <= PACK_MASK, "output escapes the domain: {y}");
                assert_eq!(deobfuscate(y, seed), p, "seed={seed} p={p}");
            }
        }
    }

    #[test]
    fn obfuscation_hides_structure() {
        // The default seed is in force.
        assert_eq!(id_seed(), DEFAULT_ID_SEED);
        // Sequential base ids do not produce visibly related codes (avalanche).
        let a = AccountId::new(100).to_string();
        let b = AccountId::new(101).to_string();
        let shared = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
        assert!(shared < 4, "codes share too long a prefix: {a} vs {b}");
        // A base account and its subaccount are likewise not obviously related.
        let main = AccountId::new(100).to_string();
        let sub = AccountId::with_sub(100, 1).to_string();
        assert_ne!(main, sub);
    }

    #[test]
    fn grouped_format_is_five_groups_of_four_and_re_parses() {
        let acc = AccountId::with_sub(5, 7);
        let grouped = acc.to_grouped();
        let groups: Vec<&str> = grouped.split(' ').collect();
        assert_eq!(groups.len(), 5);
        assert!(groups.iter().all(|g| g.len() == 4));
        // Grouped format (with spaces) and lower case both parse back.
        assert_eq!(AccountId::from_str(&grouped).unwrap(), acc);
        assert_eq!(AccountId::from_str(&grouped.to_lowercase()).unwrap(), acc);
    }

    #[test]
    fn parses_with_spaces_or_dashes_for_url_safety() {
        let acc = AccountId::with_sub(987654321, 12345);
        let machine = acc.to_string(); // 20 chars, no separators (URL-safe)
        // The same code grouped with spaces (display) or dashes (URL-safe
        // separator) parses back to the same id, as does a mixed/irregular form.
        let spaced = acc.to_grouped();
        let dashed = spaced.replace(' ', "-");
        let mixed = format!("{}-{} {}", &machine[0..4], &machine[4..12], &machine[12..]);
        for s in [&machine, &spaced, &dashed, &mixed] {
            assert_eq!(AccountId::from_str(s).unwrap(), acc, "parse {s}");
        }
    }

    #[test]
    fn from_str_rejects_bad_checksum_and_junk() {
        let good = AccountId::with_sub(5, 7).to_string();
        assert!(AccountId::from_str(&good).is_ok());

        // A helper to overwrite one character while keeping the length.
        let with_char_at = |i: usize, c: char| {
            let mut v: Vec<char> = good.chars().collect();
            v[i] = c;
            v.into_iter().collect::<String>()
        };

        // Flip the last check digit: still numeric and right length, but the
        // checksum no longer matches, so it is rejected.
        let last = good.len() - 1;
        let flipped = with_char_at(last, if good.ends_with('8') { '9' } else { '8' });
        assert!(AccountId::from_str(&flipped).is_err(), "bad checksum");

        // Structurally malformed inputs are all rejected.
        assert!(AccountId::from_str("").is_err(), "empty");
        assert!(AccountId::from_str("not-a-code").is_err(), "junk");
        assert!(AccountId::from_str(&good[..19]).is_err(), "too short");
        assert!(
            AccountId::from_str(&format!("{good}0")).is_err(),
            "too long"
        );
        // A check digit that is not a digit (the last two positions).
        assert!(
            AccountId::from_str(&with_char_at(BODY_LEN, 'A')).is_err(),
            "alpha check"
        );
        // A non-base-36 character in the body (survives space/dash stripping).
        assert!(
            AccountId::from_str(&with_char_at(5, '*')).is_err(),
            "non-base36 body"
        );
    }
}
