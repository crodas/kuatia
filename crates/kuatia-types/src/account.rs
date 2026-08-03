//! [`Account`] records and their [`AccountFlags`].

use crate::envelope::Metadata;
use crate::ids::{AccountId, BookId, DEFAULT_BOOK};
use serde::{Deserialize, Serialize};

bitflags::bitflags! {
    /// Lifecycle and balance-constraint flags for an [`Account`].
    ///
    /// Bits 0–7 are the system range: bits 0–2 carry lifecycle meaning
    /// (`FROZEN`, `CLOSED`, `INFLIGHT`), bit 3 is the balance constraint
    /// (`DEBIT_MUST_NOT_EXCEED_CREDIT`), and bits 4–7
    /// (`RESERVED_4..RESERVED_7`) are held for future system flags. Bits 8–31
    /// are the user range (`USER_0..USER_23`), meant to be combined with
    /// [`BookPolicy::allowed_flags`](crate::BookPolicy::allowed_flags) to scope
    /// which accounts may participate in a book.
    ///
    /// Every bit has a named constant so `from_bits_truncate` never discards a
    /// set bit on the storage read path.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct AccountFlags: u32 {
        /// Account may not be the source or destination of any transfer.
        const FROZEN = 1 << 0;
        /// Terminal — no further activity.
        const CLOSED = 1 << 1;
        /// Holding account for an inflight (authorize/confirm/void) transaction.
        /// Parks funds between authorize and settlement; closed once drained.
        const INFLIGHT = 1 << 2;
        /// The account's debits may never exceed its credits: its balance may
        /// not go negative and it may not hold a negative posting. When unset
        /// (the default), the account may overdraw without bound: a shortfall is
        /// covered by a negative offset posting, and the ledger records the
        /// transfer as long as it conserves value per asset.
        const DEBIT_MUST_NOT_EXCEED_CREDIT = 1 << 3;
        /// Reserved for a future system flag; not for user assignment.
        const RESERVED_4 = 1 << 4;
        /// Reserved for a future system flag; not for user assignment.
        const RESERVED_5 = 1 << 5;
        /// Reserved for a future system flag; not for user assignment.
        const RESERVED_6 = 1 << 6;
        /// Reserved for a future system flag; not for user assignment.
        const RESERVED_7 = 1 << 7;
        /// User-defined flag 0.
        const USER_0 = 1 << 8;
        /// User-defined flag 1.
        const USER_1 = 1 << 9;
        /// User-defined flag 2.
        const USER_2 = 1 << 10;
        /// User-defined flag 3.
        const USER_3 = 1 << 11;
        /// User-defined flag 4.
        const USER_4 = 1 << 12;
        /// User-defined flag 5.
        const USER_5 = 1 << 13;
        /// User-defined flag 6.
        const USER_6 = 1 << 14;
        /// User-defined flag 7.
        const USER_7 = 1 << 15;
        /// User-defined flag 8.
        const USER_8 = 1 << 16;
        /// User-defined flag 9.
        const USER_9 = 1 << 17;
        /// User-defined flag 10.
        const USER_10 = 1 << 18;
        /// User-defined flag 11.
        const USER_11 = 1 << 19;
        /// User-defined flag 12.
        const USER_12 = 1 << 20;
        /// User-defined flag 13.
        const USER_13 = 1 << 21;
        /// User-defined flag 14.
        const USER_14 = 1 << 22;
        /// User-defined flag 15.
        const USER_15 = 1 << 23;
        /// User-defined flag 16.
        const USER_16 = 1 << 24;
        /// User-defined flag 17.
        const USER_17 = 1 << 25;
        /// User-defined flag 18.
        const USER_18 = 1 << 26;
        /// User-defined flag 19.
        const USER_19 = 1 << 27;
        /// User-defined flag 20.
        const USER_20 = 1 << 28;
        /// User-defined flag 21.
        const USER_21 = 1 << 29;
        /// User-defined flag 22.
        const USER_22 = 1 << 30;
        /// User-defined flag 23.
        const USER_23 = 1 << 31;
    }
}

/// A registered entity that must exist before it can transact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Stable identity for this account (base account plus subaccount).
    pub id: AccountId,
    /// Monotonically increasing version, starts at 1 on creation.
    pub version: u64,
    /// Lifecycle and balance-constraint flags. The balance constraint lives in
    /// [`AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT`].
    pub flags: AccountFlags,
    /// Book this entity belongs to.
    pub book: BookId,
    /// Free-form key-value metadata.
    pub metadata: Metadata,
}

impl Account {
    /// Create a version-1 main-subaccount account: no flags, the default book,
    /// and empty metadata. With no flags the account may overdraw without bound
    /// (a shortfall becomes a negative offset posting); set
    /// [`AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT`] to forbid that, or use
    /// [`Account::debit_must_not_exceed_credit`]. Set the other fields
    /// explicitly when you need them.
    pub fn new(id: AccountId) -> Self {
        Self::new_ref(id)
    }

    /// Like [`Account::new`] but named for the subaccount-reference case; the
    /// signature is identical.
    pub fn new_ref(id: AccountId) -> Self {
        Self {
            id,
            version: 1,
            flags: AccountFlags::empty(),
            book: DEFAULT_BOOK,
            metadata: Metadata::new(),
        }
    }

    /// A version-1 account whose debits may never exceed its credits: its
    /// balance may not go negative and it may not hold a negative posting.
    /// Equivalent to `Account::new(id)` with
    /// [`AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT`] set.
    pub fn debit_must_not_exceed_credit(id: AccountId) -> Self {
        let mut account = Self::new(id);
        account.flags |= AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT;
        account
    }

    /// Whether this account forbids overdraft, i.e. carries the
    /// [`AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT`] flag. When `false` (the
    /// default) the account may overdraw without bound.
    pub fn forbids_overdraft(&self) -> bool {
        self.flags
            .contains(AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT)
    }

    /// Returns `true` if the account has the `FROZEN` flag set.
    pub fn is_frozen(&self) -> bool {
        self.flags.contains(AccountFlags::FROZEN)
    }

    /// Returns `true` if the account has the `CLOSED` flag set.
    pub fn is_closed(&self) -> bool {
        self.flags.contains(AccountFlags::CLOSED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_flags_cover_every_bit() {
        // Every one of the 32 bits has a named constant, so `all()` fills the
        // whole `u32` and `from_bits_truncate` can never discard a set bit.
        assert_eq!(AccountFlags::all().bits(), u32::MAX);
    }

    #[test]
    fn account_flags_bit_positions() {
        assert_eq!(AccountFlags::FROZEN.bits(), 1 << 0);
        assert_eq!(AccountFlags::INFLIGHT.bits(), 1 << 2);
        assert_eq!(AccountFlags::RESERVED_7.bits(), 1 << 7);
        assert_eq!(AccountFlags::USER_0.bits(), 1 << 8);
        assert_eq!(AccountFlags::USER_8.bits(), 1 << 16);
        assert_eq!(AccountFlags::USER_23.bits(), 1 << 31);
    }

    #[test]
    fn account_flags_high_bit_survives_signed_storage_roundtrip() {
        // The SQL backend persists flags via `bits() as i32` and reloads via
        // `from_bits_truncate(bits as u32)`. Bit 31 makes the stored i32
        // negative; this pins that the reinterpret cast is bit-preserving.
        let flags = AccountFlags::USER_23 | AccountFlags::FROZEN;
        let stored = flags.bits() as i32;
        assert!(
            stored < 0,
            "USER_23 should set the sign bit when cast to i32"
        );
        let loaded = AccountFlags::from_bits_truncate(stored as u32);
        assert_eq!(loaded, flags);
    }

    #[test]
    fn debit_must_not_exceed_credit_sets_the_flag() {
        let id = AccountId::new(100);
        let acc = Account::debit_must_not_exceed_credit(id);
        assert!(acc.forbids_overdraft());
        assert!(
            acc.flags
                .contains(AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT)
        );
        // It differs from the default only by that one flag.
        let mut expected = Account::new(id);
        expected.flags |= AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT;
        assert_eq!(acc, expected);
    }

    #[test]
    fn new_account_allows_overdraft_by_default() {
        let acc = Account::new(AccountId::new(101));
        assert!(!acc.forbids_overdraft());
        assert_eq!(acc.version, 1);
        assert_eq!(acc.flags, AccountFlags::empty());
        assert_eq!(acc.book, DEFAULT_BOOK);
        assert!(acc.metadata.is_empty());
    }

    #[test]
    fn debit_must_not_exceed_credit_bit_is_bit_3() {
        assert_eq!(AccountFlags::DEBIT_MUST_NOT_EXCEED_CREDIT.bits(), 1 << 3);
    }
}
