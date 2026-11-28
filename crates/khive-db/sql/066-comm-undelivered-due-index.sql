DROP INDEX IF EXISTS idx_comm_message_outbound_due;
CREATE INDEX idx_comm_message_outbound_due
    ON notes(namespace, kind, json_extract(properties, '$.direction'),
    substr(json_extract(properties, '$.to_actor'), 1,
           instr(json_extract(properties, '$.to_actor'), ':')),
    CASE WHEN due_source = json_extract(properties, '$.next_attempt_at')
         THEN ifnull(strict_due_key, x'') ELSE x'' END,
    created_at DESC, id ASC)
    WHERE deleted_at IS NULL
      AND (json_type(properties, '$.delivered_at') IS NULL
           OR json_type(properties, '$.delivered_at') = 'null');
