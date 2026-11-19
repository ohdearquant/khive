-- Legacy cleanup includes tombstones and rows without an explicit expiry.
-- Keep these predicates identical to the typed NoteFilter compiler: numeric
-- 1 is not JSON true, and only ASCII-space-empty text slugs are legacy.
CREATE INDEX IF NOT EXISTS idx_comm_quarantine_legacy_expiry
    ON notes(namespace, kind, json_extract(properties, '$.channel_kind'),
             COALESCE(expires_at, created_at), id)
    WHERE (json_extract(properties, '$.quarantined') = 'true'
           OR json_type(properties, '$.quarantined') = 'true')
      AND (json_type(properties, '$.channel_slug') IS NULL
           OR json_type(properties, '$.channel_slug') = 'null'
           OR (json_type(properties, '$.channel_slug') = 'text'
               AND trim(json_extract(properties, '$.channel_slug')) = ''));
