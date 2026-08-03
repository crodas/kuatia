//! The [`Envelope`] — the resolved, atomic unit produced by the saga pipeline.

use crate::ids::{AccountId, AccountSnapshotId, BookId, PostingId};
use crate::posting::NewPosting;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Free-form key→value metadata.
pub type Metadata = BTreeMap<String, Vec<u8>>;

/// The unit of atomicity — all of its consumptions and creations apply together
/// or not at all. This is the resolved, internal form produced by the saga
/// pipeline from a [`Transfer`](crate::Transfer) intent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// Posting ids consumed (spent) by this envelope.
    pub consumes: Vec<PostingId>,
    /// New postings created by this envelope.
    pub creates: Vec<NewPosting>,
    /// Account version pins for optimistic concurrency.
    pub account_snapshots: Vec<AccountSnapshotId>,
    /// Book this envelope belongs to.
    pub book: BookId,
    /// Free-form key-value metadata.
    pub metadata: Metadata,
}

impl Envelope {
    /// Posting ids consumed (spent) by this envelope.
    pub fn consumes(&self) -> &[PostingId] {
        &self.consumes
    }

    /// New postings created by this envelope.
    pub fn creates(&self) -> &[NewPosting] {
        &self.creates
    }

    /// Account version pins for optimistic concurrency.
    pub fn account_snapshots(&self) -> &[AccountSnapshotId] {
        &self.account_snapshots
    }

    /// Book this envelope belongs to.
    pub fn book(&self) -> BookId {
        self.book
    }

    /// Free-form key-value metadata.
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// Deduplicated, sorted list of account references in the created postings.
    pub fn referenced_accounts(&self) -> Vec<AccountId> {
        let mut ids: Vec<AccountId> = self.creates.iter().map(|p| p.owner).collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Set account snapshots.
    pub fn set_account_snapshots(&mut self, snapshots: Vec<AccountSnapshotId>) {
        self.account_snapshots = snapshots;
    }
}

/// Builder for constructing [`Envelope`] values.
#[derive(Default)]
pub struct EnvelopeBuilder {
    envelope: Envelope,
}

impl EnvelopeBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the posting ids to consume.
    pub fn consumes(mut self, ids: Vec<PostingId>) -> Self {
        self.envelope.consumes = ids;
        self
    }

    /// Set the new postings to create.
    pub fn creates(mut self, postings: Vec<NewPosting>) -> Self {
        self.envelope.creates = postings;
        self
    }

    /// Set the book.
    pub fn book(mut self, book: BookId) -> Self {
        self.envelope.book = book;
        self
    }

    /// Set the account version pins.
    pub fn account_snapshots(mut self, snapshots: Vec<AccountSnapshotId>) -> Self {
        self.envelope.account_snapshots = snapshots;
        self
    }

    /// Set the free-form metadata.
    pub fn metadata(mut self, metadata: Metadata) -> Self {
        self.envelope.metadata = metadata;
        self
    }

    /// Consume the builder and return the [`Envelope`].
    pub fn build(self) -> Envelope {
        self.envelope
    }
}
