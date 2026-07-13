-- V10: BlobStore content_ref reference column on entities (khive#292).
--
-- Nullable, indexed pointer into a BlobStore (khive-storage::BlobStore) —
-- a hex BLAKE3 digest, not a key buried in `properties`, so orphan-GC can
-- join on it cheaply. Storage does not validate that the referenced blob
-- exists; callers publish the blob before setting this column.

ALTER TABLE entities ADD COLUMN content_ref TEXT;

CREATE INDEX IF NOT EXISTS idx_entities_content_ref
    ON entities(content_ref)
    WHERE content_ref IS NOT NULL;
