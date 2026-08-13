//! [`CommitStore`]: the atomic write boundary (ADR-0023).
//!
//! A whole transfer, or a whole account-version transition, is applied in one
//! `BEGIN..COMMIT`. Because the write is all-or-nothing there is no half-applied
//! state for recovery to reconcile, and the three stateful guards (double-spend,
//! freeze/close, overdraft floor) are enforced inside the transaction rather than
//! best-effort before it. On Postgres the account-head reads take `FOR UPDATE`
//! before the live-posting writes; on SQLite the single write transaction
//! serializes writers and the delete-affected-count is the double-spend backstop.

use std::collections::HashSet;
use std::str::FromStr;

use async_trait::async_trait;
use sqlx::{Row, Transaction};

use kuatia_storage::error::StoreError;
use kuatia_storage::events::{LedgerEvent, event_dedup_key};
use kuatia_storage::store::{
    CommitOutcome, CommitRejection, CommitRequest, CommitStore, TransitionOutcome,
    TransitionRejection,
};
use kuatia_types::*;

use crate::SqlStore;
use crate::posting::{MAX_IDS_PER_QUERY, id_predicate};
use crate::row::{deserialize_json, envelope_id_to_hex, row_to_account, serialize_json};

fn internal(e: impl std::fmt::Display) -> StoreError {
    StoreError::Internal(e.to_string())
}

impl SqlStore {
    /// Load one account at its head version inside `tx`, taking the pessimistic
    /// lock (`FOR UPDATE` on Postgres). `None` means no such account.
    async fn locked_account(
        &self,
        tx: &mut Transaction<'_, sqlx::Any>,
        id: &AccountId,
    ) -> Result<Option<Account>, StoreError> {
        let lock = self.dialect.lock_clause();
        let row = sqlx::query(&format!(
            "SELECT a.* FROM accounts a \
             JOIN account_head h \
             ON h.id = a.id AND h.subaccount = a.subaccount AND h.version = a.version \
             WHERE h.id = $1 AND h.subaccount = $2{lock}"
        ))
        .bind(id.id)
        .bind(id.sub)
        .fetch_optional(&mut **tx)
        .await
        .map_err(internal)?;
        match row {
            Some(row) => Ok(Some(row_to_account(&row)?)),
            None => Ok(None),
        }
    }

    /// Sum the live-posting values for one `(owner, asset)` inside `tx`. All
    /// arithmetic is done in Rust with checked addition; the query never sums.
    async fn live_balance(
        &self,
        tx: &mut Transaction<'_, sqlx::Any>,
        owner: &AccountId,
        asset: &AssetId,
    ) -> Result<Cent, StoreError> {
        let rows = sqlx::query(
            "SELECT value FROM live_postings WHERE owner = $1 AND subaccount = $2 AND asset = $3",
        )
        .bind(owner.id)
        .bind(owner.sub)
        .bind(asset.0 as i32)
        .fetch_all(&mut **tx)
        .await
        .map_err(internal)?;
        let mut values = Vec::with_capacity(rows.len());
        for row in &rows {
            let v: String = row.try_get("value").map_err(internal)?;
            values.push(Cent::from_str(&v).map_err(internal)?);
        }
        Cent::checked_sum(values).map_err(internal)
    }
}

/// Append `event` inside `tx`, deduping on its key. Mirrors the standalone
/// `EventStore::append_event` but runs on the commit's transaction.
async fn append_event_tx(
    tx: &mut Transaction<'_, sqlx::Any>,
    autoid: &kuatia_types::autoid::AutoId,
    event: &LedgerEvent,
) -> Result<(), StoreError> {
    let kind_str = serde_json::to_string(&event.kind).map_err(internal)?;
    let data = serialize_json(event)?;
    let seq = autoid.next();
    match event_dedup_key(&event.kind) {
        Some(dedup_key) => {
            sqlx::query("INSERT INTO events (seq, timestamp, kind, data, dedup_key) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (dedup_key) DO NOTHING")
                .bind(seq)
                .bind(event.timestamp)
                .bind(&kind_str)
                .bind(&data)
                .bind(&dedup_key)
                .execute(&mut **tx)
                .await
                .map_err(internal)?;
        }
        None => {
            sqlx::query("INSERT INTO events (seq, timestamp, kind, data) VALUES ($1, $2, $3, $4)")
                .bind(seq)
                .bind(event.timestamp)
                .bind(&kind_str)
                .bind(&data)
                .execute(&mut **tx)
                .await
                .map_err(internal)?;
        }
    }
    Ok(())
}

/// Bind a chunk of posting ids onto a query as `(hex(transfer), idx)` pairs, in
/// the order [`id_predicate`] expects.
fn bind_ids<'q>(
    mut q: sqlx::query::Query<'q, sqlx::Any, sqlx::any::AnyArguments<'q>>,
    ids: &[PostingId],
) -> sqlx::query::Query<'q, sqlx::Any, sqlx::any::AnyArguments<'q>> {
    for id in ids {
        q = q
            .bind(envelope_id_to_hex(&id.transfer))
            .bind(id.index as i16);
    }
    q
}

#[async_trait]
impl CommitStore for SqlStore {
    async fn commit_envelope(&self, req: CommitRequest<'_>) -> Result<CommitOutcome, StoreError> {
        let lock = self.dialect.lock_clause();
        let mut tx = self.pool.begin().await.map_err(internal)?;

        let tid_hex = envelope_id_to_hex(&req.transfer_id);

        // Idempotency: an already-committed transfer returns its receipt.
        if let Some(row) = sqlx::query(&format!(
            "SELECT receipt FROM transfers WHERE id = $1{lock}"
        ))
        .bind(&tid_hex)
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?
        {
            let receipt_json: String = row.try_get("receipt").map_err(internal)?;
            return Ok(CommitOutcome::AlreadyCommitted(deserialize_json(
                &receipt_json,
            )?));
        }

        // Freeze/close guard, strict inside the transaction. Account-head reads
        // take the lock before any live-posting write (Postgres lock order).
        for aid in req.involved {
            let account = self
                .locked_account(&mut tx, aid)
                .await?
                .ok_or_else(|| StoreError::Internal(format!("commit: account {aid:?} missing")))?;
            if account.is_frozen() {
                return Ok(CommitOutcome::Rejected(CommitRejection::AccountFrozen(
                    *aid,
                )));
            }
            if account.is_closed() {
                return Ok(CommitOutcome::Rejected(CommitRejection::AccountClosed(
                    *aid,
                )));
            }
        }

        // Double-spend guard: read the still-Active consumed rows under the lock,
        // collecting their (owner, asset) for the floor check, and reject if any
        // consumed id is not live. Then delete them in the same transaction.
        let mut present: HashSet<(String, i16)> = HashSet::new();
        let mut touched: HashSet<(AccountId, AssetId)> = HashSet::new();
        for chunk in req.consume.chunks(MAX_IDS_PER_QUERY) {
            let sql = format!(
                "SELECT transfer_id, idx, owner, subaccount, asset FROM live_postings WHERE ({}) AND reservation IS NULL{lock}",
                id_predicate(chunk.len(), 1)
            );
            let rows = bind_ids(sqlx::query(&sql), chunk)
                .fetch_all(&mut *tx)
                .await
                .map_err(internal)?;
            for row in &rows {
                let transfer_id: String = row.try_get("transfer_id").map_err(internal)?;
                let idx: i16 = row.try_get("idx").map_err(internal)?;
                let owner: i64 = row.try_get("owner").map_err(internal)?;
                let subaccount: i64 = row.try_get("subaccount").map_err(internal)?;
                let asset: i32 = row.try_get("asset").map_err(internal)?;
                present.insert((transfer_id, idx));
                touched.insert((
                    AccountId::with_sub(owner, subaccount),
                    AssetId::new(asset as u32),
                ));
            }
        }
        if let Some(missing) = req
            .consume
            .iter()
            .find(|id| !present.contains(&(envelope_id_to_hex(&id.transfer), id.index as i16)))
        {
            return Ok(CommitOutcome::Rejected(CommitRejection::DoubleSpend(
                *missing,
            )));
        }
        for chunk in req.consume.chunks(MAX_IDS_PER_QUERY) {
            let sql = format!(
                "DELETE FROM live_postings WHERE ({}) AND reservation IS NULL",
                id_predicate(chunk.len(), 1)
            );
            bind_ids(sqlx::query(&sql), chunk)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
        }

        // Insert and activate the created postings.
        for p in req.create {
            let hex = envelope_id_to_hex(&p.id.transfer);
            sqlx::query(
                "INSERT INTO postings (transfer_id, idx, owner, subaccount, asset, value) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (transfer_id, idx) DO NOTHING",
            )
            .bind(&hex)
            .bind(p.id.index as i16)
            .bind(p.owner.id)
            .bind(p.owner.sub)
            .bind(p.asset.0 as i32)
            .bind(p.value.to_string())
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
            sqlx::query(
                "INSERT INTO live_postings (transfer_id, idx, owner, subaccount, asset, value, reservation) VALUES ($1, $2, $3, $4, $5, $6, NULL) ON CONFLICT (transfer_id, idx) DO NOTHING",
            )
            .bind(&hex)
            .bind(p.id.index as i16)
            .bind(p.owner.id)
            .bind(p.owner.sub)
            .bind(p.asset.0 as i32)
            .bind(p.value.to_string())
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
            touched.insert((p.owner, p.asset));
        }

        // Overdraft floor: with consumed rows deleted and created rows inserted,
        // the live set already reflects the post-commit balance. For each
        // overdraft-forbidding owner touched, its projected balance must be
        // non-negative.
        for (owner, asset) in &touched {
            let forbids = self
                .locked_account(&mut tx, owner)
                .await?
                .map(|a| a.forbids_overdraft())
                .unwrap_or(false);
            if !forbids {
                continue;
            }
            let projected = self.live_balance(&mut tx, owner, asset).await?;
            if projected.is_negative() {
                return Ok(CommitOutcome::Rejected(
                    CommitRejection::OverdraftExceeded {
                        account: *owner,
                        asset: *asset,
                        projected,
                    },
                ));
            }
        }

        // Store the transfer record and index it under the involved accounts.
        let transfer_json = serialize_json(&req.record.envelope)?;
        let receipt_json = serialize_json(&req.record.receipt)?;
        sqlx::query("INSERT INTO transfers (id, transfer, receipt, created_at, book) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (id) DO NOTHING")
            .bind(&tid_hex)
            .bind(&transfer_json)
            .bind(&receipt_json)
            .bind(req.record.created_at)
            .bind(req.record.envelope.book().0)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        for account in req.involved {
            sqlx::query("INSERT INTO transfer_accounts (transfer_id, account_id, subaccount) VALUES ($1, $2, $3) ON CONFLICT (transfer_id, account_id, subaccount) DO NOTHING")
                .bind(&tid_hex)
                .bind(account.id)
                .bind(account.sub)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
        }

        append_event_tx(&mut tx, &self.autoid, &req.event).await?;

        tx.commit().await.map_err(internal)?;
        Ok(CommitOutcome::Committed(req.record.receipt))
    }

    async fn commit_transition(
        &self,
        next: Account,
        event: LedgerEvent,
    ) -> Result<TransitionOutcome, StoreError> {
        let mut tx = self.pool.begin().await.map_err(internal)?;

        let current = self
            .locked_account(&mut tx, &next.id)
            .await?
            .ok_or_else(|| {
                StoreError::Internal(format!("transition: account {:?} missing", next.id))
            })?;

        if current.is_closed() {
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::AlreadyClosed(next.id),
            ));
        }
        if current.version >= next.version {
            // Already applied: ensure the (idempotent) event and report it.
            append_event_tx(&mut tx, &self.autoid, &event).await?;
            tx.commit().await.map_err(internal)?;
            return Ok(TransitionOutcome::AlreadyApplied);
        }
        let expected = current
            .version
            .checked_add(1)
            .ok_or_else(|| StoreError::Internal("account version overflow".to_string()))?;
        if next.version != expected {
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::VersionConflict {
                    account: next.id,
                    expected: next.version,
                },
            ));
        }

        sqlx::query(
            "INSERT INTO accounts (id, subaccount, version, flags, book, metadata) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id, subaccount, version) DO NOTHING",
        )
        .bind(next.id.id)
        .bind(next.id.sub)
        .bind(next.version as i64)
        .bind(next.flags.bits() as i32)
        .bind(next.book.0)
        .bind(serialize_json(&next.metadata)?)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;

        sqlx::query("DELETE FROM account_head WHERE id = $1 AND subaccount = $2")
            .bind(next.id.id)
            .bind(next.id.sub)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        sqlx::query("INSERT INTO account_head (id, subaccount, version) VALUES ($1, $2, $3)")
            .bind(next.id.id)
            .bind(next.id.sub)
            .bind(next.version as i64)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;

        append_event_tx(&mut tx, &self.autoid, &event).await?;

        tx.commit().await.map_err(internal)?;
        Ok(TransitionOutcome::Applied)
    }
}
