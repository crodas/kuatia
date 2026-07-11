CREATE TABLE IF NOT EXISTS active_postings (
    transfer_id TEXT NOT NULL,
    idx         SMALLINT NOT NULL,
    owner       BIGINT NOT NULL,
    subaccount  BIGINT NOT NULL DEFAULT 0,
    asset       INTEGER NOT NULL,
    value       TEXT NOT NULL,
    PRIMARY KEY (transfer_id, idx)
);

CREATE TABLE IF NOT EXISTS reserved_postings (
    transfer_id TEXT NOT NULL,
    idx         SMALLINT NOT NULL,
    owner       BIGINT NOT NULL,
    subaccount  BIGINT NOT NULL DEFAULT 0,
    asset       INTEGER NOT NULL,
    value       TEXT NOT NULL,
    reservation BIGINT NOT NULL,
    PRIMARY KEY (transfer_id, idx)
);

INSERT INTO active_postings (transfer_id, idx, owner, subaccount, asset, value) SELECT transfer_id, idx, owner, subaccount, asset, value FROM postings WHERE status = 0;

INSERT INTO reserved_postings (transfer_id, idx, owner, subaccount, asset, value, reservation) SELECT transfer_id, idx, owner, subaccount, asset, value, reservation FROM postings WHERE status = 1 AND reservation IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_active_owner ON active_postings(owner, subaccount, asset);

CREATE INDEX IF NOT EXISTS idx_reserved_owner ON reserved_postings(owner, subaccount, asset);

DROP INDEX IF EXISTS idx_postings_owner;

CREATE TABLE postings_new (
    transfer_id TEXT NOT NULL,
    idx         SMALLINT NOT NULL,
    owner       BIGINT NOT NULL,
    subaccount  BIGINT NOT NULL DEFAULT 0,
    asset       INTEGER NOT NULL,
    value       TEXT NOT NULL,
    PRIMARY KEY (transfer_id, idx)
);

INSERT INTO postings_new (transfer_id, idx, owner, subaccount, asset, value) SELECT transfer_id, idx, owner, subaccount, asset, value FROM postings;

DROP TABLE postings;

ALTER TABLE postings_new RENAME TO postings;

CREATE INDEX IF NOT EXISTS idx_postings_owner ON postings(owner, subaccount, asset);
