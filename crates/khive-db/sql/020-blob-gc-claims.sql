-- V20: durable cross-resource claims for live-traffic filesystem blob GC.
--
-- A sweep first commits a claim using SQLite-only work, then releases the
-- single-writer slot before touching the filesystem.  These triggers are the
-- fence that keeps a concurrent entity write from making a claimed object
-- newly live in that released-writer window.  Claims are removed after the
-- physical phase; a process crash leaves them fail-closed for the next sweep
-- to recover.

CREATE TABLE IF NOT EXISTS blob_gc_claims (
    root_key    TEXT    NOT NULL,
    content_ref TEXT    NOT NULL,
    claimed_at  INTEGER NOT NULL,
    PRIMARY KEY (root_key, content_ref)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_blob_gc_claims_content_ref
    ON blob_gc_claims(content_ref);

CREATE TRIGGER IF NOT EXISTS entities_reject_claimed_blob_insert
BEFORE INSERT ON entities
WHEN NEW.deleted_at IS NULL
 AND NEW.content_ref IS NOT NULL
 AND EXISTS (
     SELECT 1 FROM blob_gc_claims WHERE content_ref = NEW.content_ref
 )
BEGIN
    SELECT RAISE(ABORT, 'content_ref is reserved by an active blob sweep');
END;

CREATE TRIGGER IF NOT EXISTS entities_reject_claimed_blob_update
BEFORE UPDATE OF content_ref, deleted_at ON entities
WHEN NEW.deleted_at IS NULL
 AND NEW.content_ref IS NOT NULL
 AND EXISTS (
     SELECT 1 FROM blob_gc_claims WHERE content_ref = NEW.content_ref
 )
BEGIN
    SELECT RAISE(ABORT, 'content_ref is reserved by an active blob sweep');
END;
