SELECT namespace, id, source_id, target_id, relation, weight, created_at, updated_at, deleted_at, metadata, target_backend
FROM graph_edges WHERE namespace = ?1 AND (?2 IS NULL OR id > ?2) ORDER BY id LIMIT ?3
