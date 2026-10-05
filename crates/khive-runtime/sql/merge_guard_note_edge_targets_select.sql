SELECT e.target_id, (e.target_backend IS NULL AND EXISTS(
SELECT 1 FROM entities p WHERE p.id = e.target_id AND p.namespace = ?3
AND p.kind = ?4 AND p.deleted_at IS NULL AND p.merged_into IS NULL))
FROM graph_edges e
WHERE e.namespace = ?3 AND e.source_id = ?1 AND e.relation = ?2 AND e.deleted_at IS NULL
