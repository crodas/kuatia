CREATE TABLE IF NOT EXISTS events (
    seq       BIGINT PRIMARY KEY,
    timestamp BIGINT NOT NULL,
    kind      TEXT NOT NULL,
    data      BLOB NOT NULL
);
