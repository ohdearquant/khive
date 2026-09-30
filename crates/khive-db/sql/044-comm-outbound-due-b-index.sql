-- No application-defined function may appear in a schema index: ordinary raw
-- SQLite connections must be able to maintain notes and run integrity checks.
-- A raw properties update invalidates the app-computed key through the
-- built-in source comparison, making the row a conservative due candidate.
CREATE INDEX IF NOT EXISTS idx_comm_message_outbound_due
    ON notes(namespace, kind, json_extract(properties, '$.direction'),
    substr(json_extract(properties, '$.to_actor'), 1,
           instr(json_extract(properties, '$.to_actor'), ':')),
    CASE WHEN due_source = json_extract(properties, '$.next_attempt_at')
         THEN ifnull(strict_due_key, x'') ELSE x'' END,
    created_at DESC, id ASC)
    WHERE deleted_at IS NULL;
