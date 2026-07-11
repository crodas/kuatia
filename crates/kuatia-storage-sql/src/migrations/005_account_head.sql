CREATE TABLE IF NOT EXISTS account_head (
    id         BIGINT NOT NULL,
    subaccount BIGINT NOT NULL DEFAULT 0,
    version    BIGINT NOT NULL,
    PRIMARY KEY (id, subaccount)
);

INSERT INTO account_head (id, subaccount, version) SELECT a.id, a.subaccount, a.version FROM accounts a WHERE NOT EXISTS (SELECT 1 FROM accounts b WHERE b.id = a.id AND b.subaccount = a.subaccount AND b.version > a.version);
