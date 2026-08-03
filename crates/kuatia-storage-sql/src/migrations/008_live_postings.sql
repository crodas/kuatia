-- Merge the two hot index tables (active_postings, reserved_postings) into one
-- `live_postings` table with a nullable `reservation`: NULL = Active, set =
-- Reserved by that reservation id. A posting's state is still derived from
-- membership (present + NULL = Active, present + rid = Reserved, absent but in
-- `postings` = Spent), but the live set now lives in a single table, so a
-- spendable/live read is one index scan instead of a UNION ALL of two tables.
-- Reserve/release become a single UPDATE of `reservation` (an atomic
-- single-winner claim via `WHERE reservation IS NULL`) instead of moving a row
-- between two tables. The value table `postings` stays append-only. See ADR-0022.
CREATE TABLE live_postings (
    transfer_id TEXT NOT NULL,
    idx         SMALLINT NOT NULL,
    owner       BIGINT NOT NULL,
    subaccount  BIGINT NOT NULL DEFAULT 0,
    asset       INTEGER NOT NULL,
    value       TEXT NOT NULL,
    reservation BIGINT,
    PRIMARY KEY (transfer_id, idx)
);

INSERT INTO live_postings (transfer_id, idx, owner, subaccount, asset, value, reservation) SELECT transfer_id, idx, owner, subaccount, asset, value, NULL FROM active_postings;

INSERT INTO live_postings (transfer_id, idx, owner, subaccount, asset, value, reservation) SELECT transfer_id, idx, owner, subaccount, asset, value, reservation FROM reserved_postings;

CREATE INDEX IF NOT EXISTS idx_live_owner ON live_postings(owner, subaccount, asset);

DROP TABLE active_postings;

DROP TABLE reserved_postings;
