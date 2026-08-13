-- The write-ahead saga keyspace is gone: commits and account transitions are
-- now single atomic store transactions (ADR-0023, superseding ADR-0003), so
-- there is no half-applied state for a `sagas` write-ahead record to recover.
DROP TABLE IF EXISTS sagas;
