SELECT COUNT(*) FROM notes n
JOIN graph_edges e ON e.source_id = n.id AND e.namespace = n.namespace
WHERE n.kind = 'commit' AND n.namespace = ?1 AND n.deleted_at IS NULL
AND e.relation = 'annotates' AND e.target_id = ?2 AND e.deleted_at IS NULL
