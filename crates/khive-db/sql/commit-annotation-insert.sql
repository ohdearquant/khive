INSERT INTO graph_edges
    (namespace, id, source_id, target_id, relation, weight,
     created_at, updated_at, deleted_at, metadata, target_backend)
SELECT ?1, ?2, ?3, ?4, 'annotates', ?5, ?6, ?7, NULL, ?8, NULL
WHERE EXISTS (
    SELECT 1 FROM notes
    WHERE id = ?3 AND namespace = ?1 AND kind = 'commit'
      AND deleted_at IS NULL
      AND json_type(properties, '$.sha') = 'text'
      AND json_extract(properties, '$.sha') = ?9 COLLATE BINARY
)
AND 1 = (
    SELECT CASE WHEN (
        SELECT COUNT(*) FROM sqlite_schema
        WHERE type = 'index' AND tbl_name = 'notes'
          AND name IN ('idx_git_notes_history_canonical_sha',
                       'idx_git_notes_history_noncanonical')
    ) = 2 THEN (
        SELECT COUNT(*) FROM (
            SELECT 1 FROM notes
            WHERE namespace = ?1 AND kind = 'commit'
              AND CASE WHEN json_valid(properties) = 1
                       THEN json_type(properties, '$.sha') = 'text' ELSE 0 END
              AND json_extract(properties, '$.sha') = ?9 COLLATE BINARY
            UNION ALL
            SELECT 1 FROM notes
            WHERE namespace = ?1 AND kind = 'commit'
              AND json_valid(properties) IS NOT 1
              AND json_type(properties, '$.sha') = 'text'
              AND json_extract(properties, '$.sha') = ?9 COLLATE BINARY
        )
    ) ELSE (
        SELECT COUNT(*) FROM notes
        WHERE namespace = ?1 AND kind = 'commit'
          AND json_type(properties, '$.sha') = 'text'
          AND json_extract(properties, '$.sha') = ?9 COLLATE BINARY
    ) END
)
AND EXISTS (
    SELECT 1 FROM entities
    WHERE id = ?4 AND namespace = ?1 AND kind = 'project'
      AND deleted_at IS NULL
      AND json_type(properties, '$.repo_slug') = 'text'
      AND json_extract(properties, '$.repo_slug') = ?10 COLLATE BINARY
)
AND ?11
AND NOT EXISTS (
    SELECT 1 FROM graph_edges
    WHERE namespace = ?1 AND source_id = ?3 AND target_id = ?4
      AND relation = 'annotates'
)
ON CONFLICT DO NOTHING
