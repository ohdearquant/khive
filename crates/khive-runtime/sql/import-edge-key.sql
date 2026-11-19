SELECT namespace, id, updated_at, deleted_at FROM graph_edges
WHERE namespace = ?1 AND source_id = ?2 AND target_id = ?3 AND relation = ?4
ORDER BY id
