ALTER TABLE postings ADD COLUMN subaccount BIGINT NOT NULL DEFAULT 0;

DROP INDEX IF EXISTS idx_postings_owner;

CREATE INDEX IF NOT EXISTS idx_postings_owner ON postings(owner, subaccount, asset, status);

CREATE TABLE accounts_new (
    id          BIGINT NOT NULL,
    subaccount  BIGINT NOT NULL DEFAULT 0,
    version     BIGINT NOT NULL,
    policy      TEXT NOT NULL,
    flags       INTEGER NOT NULL,
    book        BIGINT NOT NULL,
    user_data   TEXT NOT NULL,
    metadata    TEXT NOT NULL,
    PRIMARY KEY (id, subaccount, version)
);

INSERT INTO accounts_new (id, subaccount, version, policy, flags, book, user_data, metadata) SELECT id, 0, version, policy, flags, book, user_data, metadata FROM accounts;

DROP TABLE accounts;

ALTER TABLE accounts_new RENAME TO accounts;

CREATE TABLE transfer_accounts_new (
    transfer_id TEXT NOT NULL,
    account_id  BIGINT NOT NULL,
    subaccount  BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (transfer_id, account_id, subaccount)
);

INSERT INTO transfer_accounts_new (transfer_id, account_id, subaccount) SELECT transfer_id, account_id, 0 FROM transfer_accounts;

DROP TABLE transfer_accounts;

ALTER TABLE transfer_accounts_new RENAME TO transfer_accounts;

CREATE INDEX IF NOT EXISTS idx_xfer_acct ON transfer_accounts(account_id, subaccount);
