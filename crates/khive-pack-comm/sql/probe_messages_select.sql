WITH
stats AS (
    SELECT COUNT(*) AS stale_unread_count
    FROM (
        SELECT 1
        FROM notes INDEXED BY idx_notes_unread_probe_recipient_type_direction
        WHERE notes.namespace = ?1
          AND notes.kind = 'message'
          AND notes.deleted_at IS NULL
          AND json_type(notes.properties, '$.to_actor') = 'text'
          AND ifnull(json_extract(notes.properties, '$.to_actor'), '') = ?2
          AND json_extract(notes.properties, '$.direction') = 'inbound'
          AND (json_type(notes.properties, '$.read') IS NULL
               OR json_type(notes.properties, '$.read') != 'true')
          AND notes.created_at < ?4
        LIMIT 1000
    ) AS stale_unread_rows
),
new_rows AS (
    SELECT
        notes_seq.seq AS cursor_us,
        notes.id,
        notes.created_at AS created_at_us,
        COALESCE(json_extract(notes.properties, '$.from_actor'), notes.namespace) AS from_actor,
        json_extract(notes.properties, '$.subject') AS subject
    FROM notes INDEXED BY idx_comm_message_to_actor
    JOIN notes_seq ON notes_seq.note_id = notes.id
    WHERE notes.namespace = ?1
      AND notes.kind = 'message'
      AND notes.deleted_at IS NULL
      AND json_type(notes.properties, '$.to_actor') = 'text'
      AND json_extract(notes.properties, '$.to_actor') = ?2
      AND json_extract(notes.properties, '$.direction') = 'inbound'
      AND (?3 IS NULL OR notes_seq.seq > ?3)
    ORDER BY notes_seq.seq ASC
    LIMIT 100
)
SELECT
    new_rows.cursor_us,
    stats.stale_unread_count,
    new_rows.id,
    new_rows.created_at_us,
    new_rows.from_actor,
    new_rows.subject
FROM stats
LEFT JOIN new_rows ON TRUE
ORDER BY new_rows.created_at_us ASC, new_rows.cursor_us ASC
