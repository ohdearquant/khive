SELECT source_id, deleted_at FROM graph_edges WHERE namespace=?1
AND source_id IN (SELECT value FROM json_each(?2))
AND target_id=?3 AND relation='annotates'
