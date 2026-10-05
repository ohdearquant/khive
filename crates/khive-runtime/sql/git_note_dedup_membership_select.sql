SELECT EXISTS(SELECT 1 FROM graph_edges WHERE source_id=?1 AND target_id=?2
AND namespace=?3 AND relation='annotates' AND deleted_at IS NULL
AND target_backend IS NULL) AND NOT EXISTS(SELECT 1 FROM graph_edges e
JOIN entities p ON p.id=e.target_id WHERE e.source_id=?1
AND e.relation='annotates' AND e.deleted_at IS NULL AND e.target_backend IS NULL
AND p.kind='project' AND p.deleted_at IS NULL AND p.id<>?2)
