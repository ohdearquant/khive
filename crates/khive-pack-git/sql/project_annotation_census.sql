WITH project_notes AS (
    SELECT id
    FROM notes
    WHERE namespace = ?1
      AND kind IN ('issue', 'pull_request')
      AND deleted_at IS NULL
      AND json_extract(properties, '$.project_id') = ?2
    ORDER BY id
    LIMIT ?3
)
SELECT n.id,
       CASE WHEN EXISTS (
           SELECT 1
           FROM graph_edges e
           WHERE e.namespace = ?1
             AND e.source_id = n.id
             AND e.target_id = ?2
             AND e.relation = 'annotates'
             AND e.deleted_at IS NULL
       ) THEN 0 ELSE 1 END AS missing_annotation
FROM project_notes n
