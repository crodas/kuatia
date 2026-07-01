CREATE TABLE IF NOT EXISTS accounts (
    id          BIGINT NOT NULL,
    version     BIGINT NOT NULL,
    policy      TEXT NOT NULL,
    flags       INTEGER NOT NULL,
    book        BIGINT NOT NULL,
    user_data   TEXT NOT NULL,
    metadata    TEXT NOT NULL,
    PRIMARY KEY (id, version)
);

CREATE TABLE IF NOT EXISTS postings (
    transfer_id TEXT NOT NULL,
    idx         SMALLINT NOT NULL,
    owner       BIGINT NOT NULL,
    asset       INTEGER NOT NULL,
    value       TEXT NOT NULL,
    status      SMALLINT NOT NULL,
    reservation BIGINT,
    PRIMARY KEY (transfer_id, idx)
);

CREATE INDEX IF NOT EXISTS idx_postings_owner ON postings(owner, asset, status);

CREATE TABLE IF NOT EXISTS transfers (
    id         TEXT PRIMARY KEY,
    transfer   TEXT NOT NULL,
    receipt    TEXT NOT NULL,
    created_at BIGINT NOT NULL DEFAULT 0,
    book       BIGINT NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_transfers_created_at ON transfers(created_at);
CREATE INDEX IF NOT EXISTS idx_transfers_book ON transfers(book);

CREATE TABLE IF NOT EXISTS transfer_accounts (
    transfer_id TEXT NOT NULL,
    account_id  BIGINT NOT NULL,
    PRIMARY KEY (transfer_id, account_id)
);

CREATE INDEX IF NOT EXISTS idx_xfer_acct ON transfer_accounts(account_id);

CREATE TABLE IF NOT EXISTS sagas (
    id   BIGINT PRIMARY KEY,
    data TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS events (
    seq       BIGINT PRIMARY KEY,
    timestamp BIGINT NOT NULL,
    kind      TEXT NOT NULL,
    data      TEXT NOT NULL,
    dedup_key TEXT UNIQUE
);

CREATE TABLE IF NOT EXISTS books (
    id   BIGINT PRIMARY KEY,
    name TEXT NOT NULL,
    data TEXT NOT NULL
);
