CREATE TABLE IF NOT EXISTS books (
    id       BIGINT PRIMARY KEY,
    name     TEXT NOT NULL,
    data     BLOB NOT NULL
);
