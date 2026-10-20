SELECT namespace, id, source_id, target_id, relation, created_at, updated_at, deleted_at
FROM graph_edges WHERE id = ?1 ORDER BY namespace
