-- Typed discriminator for the write-ahead saga keyspace. Both commit sagas and
-- account transitions share the sagas table, so kind records which a row is and
-- recovery dispatches by a typed column instead of an in-band blob tag. The id
-- stays the primary key (all ids come from one generator, so it is unique across
-- kinds). The default keeps any surviving row valid as an envelope commit.
ALTER TABLE sagas ADD COLUMN kind TEXT NOT NULL DEFAULT 'envelope';
