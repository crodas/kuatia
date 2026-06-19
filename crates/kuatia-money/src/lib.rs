//! Monetary amounts for the Kuatia ledger.
//!
//! [`Cent`] is a signed amount in an asset's smallest unit. It wraps an integer
//! whose width is an internal detail: the public API never names the backing
//! type, and no serialized form reveals it. The backing is chosen once at
//! compile time through the [`Backing`] alias, which defaults to `i64` and
//! switches to `i128` under the `i128` cargo feature. Adding a new width is a
//! single [`CentBacking`] impl plus one line on [`Backing`]; nothing downstream
//! changes.
//!
//! All arithmetic is checked: addition, subtraction and negation return
//! [`OverflowError`] rather than wrapping, so the ledger's conservation sum can
//! never silently round or overflow.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

// ---------------------------------------------------------------------------
// Backing selection
// ---------------------------------------------------------------------------

/// The integer type backing every [`Cent`]. `i64` by default; `i128` under the
/// `i128` cargo feature. This is the single point where the money width is
/// chosen, and it is never named in a public signature.
#[cfg(not(feature = "i128"))]
pub type Backing = i64;

/// The integer type backing every [`Cent`]. `i64` by default; `i128` under the
/// `i128` cargo feature. This is the single point where the money width is
/// chosen, and it is never named in a public signature.
#[cfg(feature = "i128")]
pub type Backing = i128;

// ---------------------------------------------------------------------------
// CentBacking — the swap surface
// ---------------------------------------------------------------------------

/// The contract an integer must satisfy to back a [`Cent`]. Implemented for
/// `i64` and `i128`; implement it for another integer to add a new money width.
///
/// It carries only the width-dependent primitives [`Cent`] needs (canonical
/// 16-byte widening, decimal scaling, parsing, and absolute division). Plain
/// checked add/sub/neg are used directly on the concrete backing.
pub trait CentBacking: Copy + Ord + Default + fmt::Display {
    /// The additive identity (zero) for this backing.
    const ZERO: Self;

    /// Ten raised to `exp`, or `None` on overflow. Used to scale decimals.
    fn ten_pow(exp: u32) -> Option<Self>;

    /// Parse a base-10 signed integer string, or `None` if it is not valid.
    fn parse_str(s: &str) -> Option<Self>;

    /// Widen to `i128` for the fixed-width canonical encoding.
    fn to_i128(self) -> i128;

    /// Narrow from `i128`, or `None` if the value does not fit this backing.
    fn try_from_i128(v: i128) -> Option<Self>;

    /// Divide the absolute value of `self` by the absolute value of `d`,
    /// returning `(quotient, remainder)` as unsigned `u128`. Returns `(0, 0)`
    /// when `d` is zero. Used to split a value into whole and fractional parts
    /// for display.
    fn div_rem_abs(self, d: Self) -> (u128, u128);
}

impl CentBacking for i64 {
    const ZERO: Self = 0;

    fn ten_pow(exp: u32) -> Option<Self> {
        10i64.checked_pow(exp)
    }

    fn parse_str(s: &str) -> Option<Self> {
        s.parse().ok()
    }

    fn to_i128(self) -> i128 {
        self as i128
    }

    fn try_from_i128(v: i128) -> Option<Self> {
        i64::try_from(v).ok()
    }

    fn div_rem_abs(self, d: Self) -> (u128, u128) {
        let dd = d.unsigned_abs() as u128;
        if dd == 0 {
            return (0, 0);
        }
        let a = self.unsigned_abs() as u128;
        (a / dd, a % dd)
    }
}

impl CentBacking for i128 {
    const ZERO: Self = 0;

    fn ten_pow(exp: u32) -> Option<Self> {
        10i128.checked_pow(exp)
    }

    fn parse_str(s: &str) -> Option<Self> {
        s.parse().ok()
    }

    fn to_i128(self) -> i128 {
        self
    }

    fn try_from_i128(v: i128) -> Option<Self> {
        Some(v)
    }

    fn div_rem_abs(self, d: Self) -> (u128, u128) {
        let dd = d.unsigned_abs();
        if dd == 0 {
            return (0, 0);
        }
        let a = self.unsigned_abs();
        (a / dd, a % dd)
    }
}

// ---------------------------------------------------------------------------
// Cent — stored monetary amount
// ---------------------------------------------------------------------------

/// A monetary amount in the smallest unit of one asset (cents, satoshis, …).
///
/// The backing integer is private and its width is hidden: read a `Cent` with
/// [`Display`](fmt::Display)/[`to_string`](ToString::to_string) or parse one
/// with [`FromStr`], compare with [`Ord`], and do arithmetic only through the
/// checked methods. Serde round-trips it as a string, so no serialized form
/// reveals the width.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Cent(Backing);

/// Returned when a [`Cent`] arithmetic operation would overflow or underflow,
/// or when a value does not fit the active backing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverflowError;

impl fmt::Display for OverflowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "monetary amount overflow")
    }
}

impl std::error::Error for OverflowError {}

/// Returned when a string cannot be parsed into a [`Cent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseCentError;

impl fmt::Display for ParseCentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid monetary amount")
    }
}

impl std::error::Error for ParseCentError {}

impl Cent {
    /// The zero amount.
    pub const ZERO: Cent = Cent(0);

    /// Returns `true` if the amount is strictly positive.
    pub fn is_positive(self) -> bool {
        self.0 > 0
    }

    /// Returns `true` if the amount is strictly negative.
    pub fn is_negative(self) -> bool {
        self.0 < 0
    }

    /// Returns `true` if the amount is zero.
    pub fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Checked addition, returning [`OverflowError`] on overflow.
    pub fn checked_add(self, rhs: Self) -> Result<Self, OverflowError> {
        self.0.checked_add(rhs.0).map(Cent).ok_or(OverflowError)
    }

    /// Checked subtraction, returning [`OverflowError`] on underflow.
    pub fn checked_sub(self, rhs: Self) -> Result<Self, OverflowError> {
        self.0.checked_sub(rhs.0).map(Cent).ok_or(OverflowError)
    }

    /// Checked negation, returning [`OverflowError`] at the backing's minimum.
    pub fn checked_neg(self) -> Result<Self, OverflowError> {
        self.0.checked_neg().map(Cent).ok_or(OverflowError)
    }

    /// Sum an iterator of `Cent` values with overflow checking.
    pub fn checked_sum(iter: impl IntoIterator<Item = Self>) -> Result<Self, OverflowError> {
        let mut sum = Cent::ZERO;
        for x in iter {
            sum = sum.checked_add(x)?;
        }
        Ok(sum)
    }

    /// The canonical 16-byte big-endian encoding (sign-extended), used for
    /// content-addressed hashing. The width is fixed regardless of the backing,
    /// so the same amount hashes identically under any backing.
    pub fn to_canonical_bytes(self) -> [u8; 16] {
        self.0.to_i128().to_be_bytes()
    }

    /// Decode a [`Cent`] from its canonical 16-byte encoding, returning
    /// [`OverflowError`] if the value does not fit the active backing.
    pub fn from_canonical_bytes(bytes: &[u8; 16]) -> Result<Self, OverflowError> {
        Backing::try_from_i128(i128::from_be_bytes(*bytes))
            .map(Cent)
            .ok_or(OverflowError)
    }
}

impl From<i64> for Cent {
    fn from(v: i64) -> Self {
        Cent(v as Backing)
    }
}

impl From<i32> for Cent {
    fn from(v: i32) -> Self {
        Cent(v as Backing)
    }
}

impl From<u32> for Cent {
    fn from(v: u32) -> Self {
        Cent(v as Backing)
    }
}

impl From<u8> for Cent {
    fn from(v: u8) -> Self {
        Cent(v as Backing)
    }
}

impl From<i8> for Cent {
    fn from(v: i8) -> Self {
        Cent(v as Backing)
    }
}

impl fmt::Debug for Cent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Cent({})", self.0)
    }
}

impl fmt::Display for Cent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for Cent {
    type Err = ParseCentError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Backing::parse_str(s).map(Cent).ok_or(ParseCentError)
    }
}

impl Serialize for Cent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Cent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Amount — human-friendly parser/formatter (not stored)
// ---------------------------------------------------------------------------

/// Parses and formats human-readable amounts with a fixed number of decimal
/// places. NOT stored anywhere — used only to convert between strings and
/// [`Cent`] values.
pub struct Amount {
    decimals: u8,
}

/// Error returned when parsing an amount string fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseAmountError {
    /// The input string is not a valid number.
    InvalidFormat(String),
    /// Too many decimal places for the configured precision.
    TooManyDecimals {
        /// Maximum allowed decimal places.
        max: u8,
        /// Number of decimal places found in the input.
        found: usize,
    },
}

impl fmt::Display for ParseAmountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFormat(s) => write!(f, "invalid amount format: {s}"),
            Self::TooManyDecimals { max, found } => {
                write!(f, "too many decimals: max {max}, found {found}")
            }
        }
    }
}

impl std::error::Error for ParseAmountError {}

impl Amount {
    /// Create an `Amount` formatter with the given number of decimal places.
    pub fn new(decimals: u8) -> Self {
        Self { decimals }
    }

    /// Parses a decimal string into a [`Cent`] value.
    pub fn parse(&self, s: &str) -> Result<Cent, ParseAmountError> {
        let s = s.trim();
        let (negative, s) = if let Some(rest) = s.strip_prefix('-') {
            (true, rest)
        } else {
            (false, s)
        };

        let (whole_str, frac_str) = if let Some((w, f)) = s.split_once('.') {
            (w, f)
        } else {
            (s, "")
        };

        if whole_str.is_empty() && frac_str.is_empty() {
            return Err(ParseAmountError::InvalidFormat(s.to_string()));
        }

        let whole: Backing = if whole_str.is_empty() {
            Backing::ZERO
        } else {
            Backing::parse_str(whole_str)
                .ok_or_else(|| ParseAmountError::InvalidFormat(s.to_string()))?
        };

        if frac_str.len() > self.decimals as usize {
            return Err(ParseAmountError::TooManyDecimals {
                max: self.decimals,
                found: frac_str.len(),
            });
        }

        if !frac_str.is_empty() && !frac_str.chars().all(|c| c.is_ascii_digit()) {
            return Err(ParseAmountError::InvalidFormat(s.to_string()));
        }

        let frac: Backing = if frac_str.is_empty() {
            Backing::ZERO
        } else {
            let padded = format!("{:0<width$}", frac_str, width = self.decimals as usize);
            Backing::parse_str(&padded)
                .ok_or_else(|| ParseAmountError::InvalidFormat(s.to_string()))?
        };

        let multiplier = Backing::ten_pow(self.decimals as u32)
            .ok_or_else(|| ParseAmountError::InvalidFormat(s.to_string()))?;
        let value = whole
            .checked_mul(multiplier)
            .and_then(|v| v.checked_add(frac))
            .ok_or_else(|| ParseAmountError::InvalidFormat(s.to_string()))?;

        let value = if negative {
            value
                .checked_neg()
                .ok_or_else(|| ParseAmountError::InvalidFormat(s.to_string()))?
        } else {
            value
        };
        Ok(Cent(value))
    }

    /// Formats a [`Cent`] value as a decimal string.
    pub fn format(&self, cent: Cent) -> String {
        if self.decimals == 0 {
            return cent.to_string();
        }

        let Some(multiplier) = Backing::ten_pow(self.decimals as u32) else {
            return cent.to_string();
        };

        let negative = cent.is_negative();
        let (whole, frac) = cent.0.div_rem_abs(multiplier);

        let sign = if negative { "-" } else { "" };
        format!(
            "{sign}{whole}.{frac:0>width$}",
            width = self.decimals as usize
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_math_overflows_to_error() {
        let max = Cent::from_str(&Backing::MAX.to_string()).unwrap();
        assert_eq!(max.checked_add(Cent::from(1)), Err(OverflowError));
        let min = Cent::from_str(&Backing::MIN.to_string()).unwrap();
        assert_eq!(min.checked_neg(), Err(OverflowError));
        assert_eq!(Cent::from(2).checked_sub(Cent::from(5)), Ok(Cent::from(-3)));
    }

    #[test]
    fn string_round_trip() {
        for v in [-1234i64, 0, 1, 500, i64::from(i32::MAX)] {
            let c = Cent::from(v);
            assert_eq!(Cent::from_str(&c.to_string()), Ok(c));
        }
    }

    #[test]
    fn serde_is_a_string() {
        let c = Cent::from(-50);
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(json, "\"-50\"");
        assert_eq!(serde_json::from_str::<Cent>(&json).unwrap(), c);
    }

    #[test]
    fn canonical_bytes_are_16_and_round_trip() {
        let c = Cent::from(500);
        let bytes = c.to_canonical_bytes();
        assert_eq!(bytes.len(), 16);
        assert_eq!(Cent::from_canonical_bytes(&bytes), Ok(c));
    }

    #[test]
    fn canonical_bytes_are_width_independent() {
        // 500 encodes the same 16 bytes whether the backing is i64 or i128.
        let expected = 500i128.to_be_bytes();
        assert_eq!(Cent::from(500).to_canonical_bytes(), expected);
    }

    #[test]
    fn from_canonical_bytes_rejects_out_of_range() {
        // A value larger than i64::MAX only fits when the backing is i128.
        let big = (i128::from(i64::MAX)) + 1;
        let bytes = big.to_be_bytes();
        let decoded = Cent::from_canonical_bytes(&bytes);
        if cfg!(feature = "i128") {
            assert!(decoded.is_ok());
        } else {
            assert_eq!(decoded, Err(OverflowError));
        }
    }

    #[test]
    fn amount_parse_format_round_trip() {
        let amt = Amount::new(2);
        assert_eq!(amt.parse("12.34").unwrap(), Cent::from(1234));
        assert_eq!(amt.parse("-0.05").unwrap(), Cent::from(-5));
        assert_eq!(amt.format(Cent::from(1234)), "12.34");
        assert_eq!(amt.format(Cent::from(-5)), "-0.05");
        assert_eq!(Amount::new(0).format(Cent::from(700)), "700");
    }
}
