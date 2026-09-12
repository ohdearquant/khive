SELECT e.source_id AS source_id, e.target_id AS target_id
FROM graph_edges e
JOIN entities t ON t.id = e.target_id
WHERE e.relation = 'depends_on' AND e.deleted_at IS NULL AND t.entity_type = 'module'
ORDER BY e.target_id, e.source_id;
