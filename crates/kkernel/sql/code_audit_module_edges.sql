SELECT e.id AS id, e.source_id AS source_id, e.target_id AS target_id,
       s.name AS source_name, t.name AS target_name
FROM graph_edges e
JOIN entities s ON s.id = e.source_id
JOIN entities t ON t.id = e.target_id
WHERE e.relation = 'depends_on' AND e.deleted_at IS NULL
  AND s.entity_type = 'module' AND t.entity_type = 'module'
ORDER BY e.source_id, e.target_id, e.id;
