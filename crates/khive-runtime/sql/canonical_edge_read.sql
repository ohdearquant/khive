SELECT id, namespace, source_id, target_id, relation, weight,
       created_at, updated_at, deleted_at, metadata, target_backend
FROM graph_edges WHERE id = ?1
