-- Append-only balance cache points (ADR-0019). A derived, rebuildable read
-- accelerator: each row is one snapshot of a (account, subaccount, asset)
-- balance plus the commit-time watermark (unix millis) it covers, tagged with a
-- Rust-minted monotonic id. Rows are only ever inserted, never updated. A read
-- selects the highest id for the (account, subaccount, asset). The balance is a
-- Cent stored as TEXT, like every other monetary column, and the store never
-- does arithmetic on it.
CREATE TABLE balance_projection (
    id         BIGINT  NOT NULL,
    account    BIGINT  NOT NULL,
    subaccount BIGINT  NOT NULL DEFAULT 0,
    asset      INTEGER NOT NULL,
    balance    TEXT    NOT NULL,
    watermark  BIGINT  NOT NULL,
    PRIMARY KEY (id)
);
-- The closest-at-or-before read filters by (account, subaccount, asset) and
-- watermark, ordering by watermark then id, so the index leads with those.
CREATE INDEX idx_balance_projection_closest
    ON balance_projection (account, subaccount, asset, watermark, id);
