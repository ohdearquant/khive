-- Preserve rejected roles and their blob ownership before tightening the table.
CREATE TABLE IF NOT EXISTS attachment_quarantine (
    record_uuid TEXT NOT NULL,
    substrate   TEXT NOT NULL,
    role        TEXT NOT NULL,
    content_ref TEXT NOT NULL,
    media_type  TEXT,
    size_bytes  INTEGER,
    created_at  INTEGER NOT NULL,
    reason      TEXT NOT NULL CHECK (reason = 'invalid_role'),
    PRIMARY KEY (record_uuid, role)
) STRICT;

INSERT INTO attachment_quarantine
    (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at, reason)
SELECT record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at, 'invalid_role'
FROM attachments
WHERE NOT (
    length(role) > 0
    AND instr(role, char(0)) = 0
    AND role NOT GLOB ('*[' || char(1) || '-' || char(31) || char(127) || '-' || char(159) || ']*')
);

-- Isolated historical fixtures may contain only the V21 staging DDL, and a
-- ledger-tail replay may retain V47 quarantine objects. Always reinstall the
-- canonical fences after rebuilding the live table; preserve quarantine rows.
DROP TRIGGER IF EXISTS attachments_reject_claimed_blob_insert;
DROP TRIGGER IF EXISTS attachments_reject_claimed_blob_update;
DROP TRIGGER IF EXISTS attachment_quarantine_reject_claimed_blob_insert;
DROP TRIGGER IF EXISTS attachment_quarantine_reject_claimed_blob_update;

CREATE TABLE attachments_strict_role (
    record_uuid TEXT NOT NULL,
    substrate   TEXT NOT NULL CHECK (substrate IN ('entity', 'note')),
    role        TEXT NOT NULL
        CHECK (
            length(role) > 0
            AND instr(role, char(0)) = 0
            AND role NOT GLOB ('*[' || char(1) || '-' || char(31) || char(127) || '-' || char(159) || ']*')
        ),
    content_ref TEXT NOT NULL
        CHECK (
            length(content_ref) = 64
            AND content_ref NOT GLOB '*[^0-9a-f]*'
            -- length() and GLOB both stop scanning at an embedded NUL, so a
            -- value of 64 hex characters followed by a NUL and arbitrary
            -- trailing bytes would otherwise satisfy both arms above. This
            -- blob-cast comparison scales with the database's text encoding
            -- (64 bytes in UTF-8, 128 in UTF-16) and a NUL-tailed value's
            -- blob cast keeps its full byte tail, so it diverges and fails.
            AND length(CAST(content_ref AS BLOB))
                = length(CAST('0000000000000000000000000000000000000000000000000000000000000000' AS BLOB))
        ),
    media_type  TEXT,
    size_bytes  INTEGER CHECK (size_bytes IS NULL OR size_bytes >= 0),
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (record_uuid, role)
) STRICT;

INSERT INTO attachments_strict_role
    (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at)
SELECT record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at
FROM attachments
WHERE length(role) > 0
    AND instr(role, char(0)) = 0
    AND role NOT GLOB ('*[' || char(1) || '-' || char(31) || char(127) || '-' || char(159) || ']*');

DROP TABLE attachments;
ALTER TABLE attachments_strict_role RENAME TO attachments;

CREATE INDEX IF NOT EXISTS idx_attachments_content_ref
    ON attachments(content_ref);
CREATE INDEX IF NOT EXISTS idx_attachment_quarantine_content_ref
    ON attachment_quarantine(content_ref);

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

CREATE TRIGGER attachment_quarantine_reject_claimed_blob_insert
BEFORE INSERT ON attachment_quarantine
WHEN EXISTS (
    SELECT 1 FROM blob_gc_claims WHERE content_ref = NEW.content_ref
)
BEGIN
    SELECT RAISE(ABORT, 'content_ref is reserved by an active blob sweep');
END;

CREATE TRIGGER attachment_quarantine_reject_claimed_blob_update
BEFORE UPDATE OF content_ref ON attachment_quarantine
WHEN EXISTS (
    SELECT 1 FROM blob_gc_claims WHERE content_ref = NEW.content_ref
)
BEGIN
    SELECT RAISE(ABORT, 'content_ref is reserved by an active blob sweep');
END;
