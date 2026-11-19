SELECT EXISTS (
    SELECT 1 FROM notes
    WHERE id = ?1 AND namespace = ?2 AND kind = 'commit'
      AND deleted_at IS NULL
      AND json_type(properties, '$.sha') = 'text'
      AND json_extract(properties, '$.sha') = ?3 COLLATE BINARY
      AND 1 = (
        SELECT CASE WHEN (
            SELECT COUNT(*) FROM sqlite_schema
            WHERE type = 'index' AND tbl_name = 'notes'
              AND name IN ('idx_git_notes_history_canonical_sha',
                           'idx_git_notes_history_noncanonical')
        ) = 2 THEN (
            SELECT COUNT(*) FROM (
                SELECT 1 FROM notes
                WHERE namespace = ?2 AND kind = 'commit'
                  AND CASE WHEN json_valid(properties) = 1
                           THEN json_type(properties, '$.sha') = 'text' ELSE 0 END
                  AND json_extract(properties, '$.sha') = ?3 COLLATE BINARY
                UNION ALL
                SELECT 1 FROM notes
                WHERE namespace = ?2 AND kind = 'commit'
                  AND json_valid(properties) IS NOT 1
                  AND json_type(properties, '$.sha') = 'text'
                  AND json_extract(properties, '$.sha') = ?3 COLLATE BINARY
            )
        ) ELSE (
            SELECT COUNT(*) FROM notes
            WHERE namespace = ?2 AND kind = 'commit'
              AND json_type(properties, '$.sha') = 'text'
              AND json_extract(properties, '$.sha') = ?3 COLLATE BINARY
        ) END
      )
)
