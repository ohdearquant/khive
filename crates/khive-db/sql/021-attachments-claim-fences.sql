-- V21 final claim fences.  These replace V20's entities.content_ref fences
-- in the same exclusive transaction that drops the legacy column.

CREATE TRIGGER attachments_reject_claimed_blob_insert
BEFORE INSERT ON attachments
WHEN EXISTS (
    SELECT 1 FROM blob_gc_claims WHERE content_ref = NEW.content_ref
)
BEGIN
    SELECT RAISE(ABORT, 'content_ref is reserved by an active blob sweep');
END;

CREATE TRIGGER attachments_reject_claimed_blob_update
BEFORE UPDATE OF content_ref ON attachments
WHEN EXISTS (
    SELECT 1 FROM blob_gc_claims WHERE content_ref = NEW.content_ref
)
BEGIN
    SELECT RAISE(ABORT, 'content_ref is reserved by an active blob sweep');
END;
