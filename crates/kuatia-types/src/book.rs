//! [`Book`] — the transfer-policy scope gating account and asset participation.

use crate::account::AccountFlags;
use crate::ids::{AccountId, AssetId, BookId};
use serde::{Deserialize, Serialize};

/// A Book is a transfer policy scope: it gates which accounts and assets may
/// participate in a transfer. It is **not** the chronological entry log (the
/// transfer log plays that role), and it does **not** partition balances —
/// balances are global; a Book only gates participation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Book {
    /// Stable identity for this book.
    pub id: BookId,
    /// Human-readable name.
    pub name: String,
    /// Participation rules for this book.
    pub policy: BookPolicy,
}

/// The participation rules for a [`Book`]. An empty field means "no restriction".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookPolicy {
    /// If non-empty, only these assets may appear in movements.
    pub allowed_assets: Vec<AssetId>,
    /// If non-empty, accounts with ANY of these flags may participate.
    pub allowed_flags: AccountFlags,
    /// If non-empty, these specific accounts may participate (in addition to flag matches).
    pub allowed_accounts: Vec<AccountId>,
}

/// Builder for constructing [`Book`] values.
pub struct BookBuilder {
    book: Book,
}

impl BookBuilder {
    /// Create a new book builder with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            book: Book {
                id: BookId::generate(),
                name: name.into(),
                policy: BookPolicy {
                    allowed_assets: Vec::new(),
                    allowed_flags: AccountFlags::empty(),
                    allowed_accounts: Vec::new(),
                },
            },
        }
    }

    /// Set the book id explicitly.
    pub fn id(mut self, id: BookId) -> Self {
        self.book.id = id;
        self
    }

    /// Add an allowed asset.
    pub fn allow_asset(mut self, asset: AssetId) -> Self {
        self.book.policy.allowed_assets.push(asset);
        self
    }

    /// Set allowed account flags — accounts with ANY of these flags may participate.
    pub fn allow_flags(mut self, flags: AccountFlags) -> Self {
        self.book.policy.allowed_flags = flags;
        self
    }

    /// Add a specific allowed account.
    pub fn allow_account(mut self, account: AccountId) -> Self {
        self.book.policy.allowed_accounts.push(account);
        self
    }

    /// Consume the builder and return the [`Book`].
    pub fn build(self) -> Book {
        self.book
    }
}
