SELECT CASE WHEN EXISTS (
           SELECT 1
           FROM graph_edges e
           WHERE e.namespace = ?2
             AND e.source_id = n.id
             AND e.target_id = ?5
             AND e.relation = 'annotates'
             AND e.deleted_at IS NULL
       ) THEN 0 ELSE 1 END AS missing_annotation
FROM notes n
WHERE n.id = ?1
  AND n.namespace = ?2
  AND n.kind = ?3
  AND n.kind IN ('issue', 'pull_request')
  AND n.deleted_at IS NULL
  AND json_extract(n.properties, '$.number') = ?4
  AND json_extract(n.properties, '$.project_id') = ?5
