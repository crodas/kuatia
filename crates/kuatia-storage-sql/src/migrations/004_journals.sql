CREATE TABLE IF NOT EXISTS journals (
    id       BIGINT PRIMARY KEY,
    name     TEXT NOT NULL,
    data     BLOB NOT NULL
);
