-- The overdraft/balance policy is no longer a separate column. The single
-- balance constraint (debit must not exceed credit) now lives in the account
-- flags bitfield, so the `policy` column is dropped. Both SQLite (>= 3.35) and
-- PostgreSQL support ALTER TABLE ... DROP COLUMN.
ALTER TABLE accounts DROP COLUMN policy;
