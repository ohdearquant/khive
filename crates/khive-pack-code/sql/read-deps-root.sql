SELECT id,
       CASE WHEN kind = 'project' THEN 'project' ELSE entity_type END AS projected_kind,
       name
FROM entities
WHERE id = ?1
  AND deleted_at IS NULL
  AND (kind = 'project'
       OR (kind = 'concept' AND entity_type IN ('module', 'function', 'datatype', 'interface')))
