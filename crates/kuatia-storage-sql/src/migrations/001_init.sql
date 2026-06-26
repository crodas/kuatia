CREATE TABLE IF NOT EXISTS accounts (
    id          BIGINT NOT NULL,
    version     BIGINT NOT NULL,
    policy      TEXT NOT NULL,
    flags       INTEGER NOT NULL,
    book     BIGINT NOT NULL,
    user_data   BLOB NOT NULL,
    metadata    BLOB NOT NULL,
    PRIMARY KEY (id, version)
);

CREATE TABLE IF NOT EXISTS postings (
    transfer_id BLOB NOT NULL,
    idx         SMALLINT NOT NULL,
    owner       BIGINT NOT NULL,
    asset       INTEGER NOT NULL,
    value       BIGINT NOT NULL,
    status      SMALLINT NOT NULL,
    PRIMARY KEY (transfer_id, idx)
);

CREATE INDEX IF NOT EXISTS idx_postings_owner ON postings(owner, asset, status);

CREATE TABLE IF NOT EXISTS transfers (
    id       BLOB PRIMARY KEY,
    transfer BLOB NOT NULL,
    receipt  BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS transfer_accounts (
    transfer_id BLOB NOT NULL,
    account_id  BIGINT NOT NULL,
    PRIMARY KEY (transfer_id, account_id)
);

CREATE INDEX IF NOT EXISTS idx_xfer_acct ON transfer_accounts(account_id);

CREATE TABLE IF NOT EXISTS sagas (
    id   BIGINT PRIMARY KEY,
    data BLOB NOT NULL
);
