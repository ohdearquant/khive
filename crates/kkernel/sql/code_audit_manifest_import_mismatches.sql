SELECT e.id AS id, s.name AS source_name, t.name AS target_name
FROM graph_edges e
JOIN entities s ON s.id = e.source_id
JOIN entities t ON t.id = e.target_id
WHERE e.relation = 'depends_on' AND e.deleted_at IS NULL
  AND EXISTS (
    SELECT 1 FROM json_each(e.metadata, '$.dependency_kinds') WHERE value = 'import'
  )
  AND NOT EXISTS (
    SELECT 1 FROM json_each(e.metadata, '$.dependency_kinds') WHERE value <> 'import'
  )
ORDER BY e.source_id, e.target_id, e.id;
