SELECT EXISTS(SELECT 1 FROM graph_edges WHERE namespace = ?1 AND relation = ?2
AND source_id = ?3 AND target_id = ?4
AND deleted_at IS NULL AND target_backend IS NULL)
