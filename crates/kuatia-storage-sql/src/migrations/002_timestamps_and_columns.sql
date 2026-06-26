ALTER TABLE transfers ADD COLUMN created_at BIGINT NOT NULL DEFAULT 0;
ALTER TABLE transfers ADD COLUMN book BIGINT NOT NULL DEFAULT 0;
CREATE INDEX idx_transfers_created_at ON transfers(created_at);
CREATE INDEX idx_transfers_book ON transfers(book);
