SELECT m.id AS id, m.name AS name,
       json_extract(m.properties,'$.source_project') AS source_project
FROM entities m
WHERE m.entity_type = 'module' AND m.deleted_at IS NULL
  AND NOT EXISTS (
    SELECT 1 FROM graph_edges e
    WHERE e.relation = 'depends_on' AND e.deleted_at IS NULL AND e.target_id = m.id
  )
ORDER BY m.name, m.id;
