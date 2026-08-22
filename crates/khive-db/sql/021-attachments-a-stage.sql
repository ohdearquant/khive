-- V21 stage 1: durable, resumable attachments-first cutover (ADR-121/ADR-160).
--
-- The boot coordinator executes this DDL while holding the canonical database
-- GC owner.  It deliberately does not remove entities.content_ref or its V20
-- claim fences.  Those remain authoritative until the application migrator
-- has verified pack-owned roles and the final transaction commits.

CREATE TABLE IF NOT EXISTS attachments (
    record_uuid TEXT NOT NULL,
    substrate   TEXT NOT NULL CHECK (substrate IN ('entity', 'note')),
    role        TEXT NOT NULL CHECK (length(role) > 0),
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

CREATE INDEX IF NOT EXISTS idx_attachments_content_ref
    ON attachments(content_ref);

CREATE TABLE IF NOT EXISTS attachment_cutover_state (
    singleton    INTEGER PRIMARY KEY CHECK (singleton = 1),
    state        TEXT NOT NULL CHECK (state IN ('incomplete', 'complete')),
    started_at   INTEGER NOT NULL,
    completed_at INTEGER,
    CHECK (
        (state = 'incomplete' AND completed_at IS NULL)
        OR (state = 'complete' AND completed_at IS NOT NULL)
    )
) STRICT;
