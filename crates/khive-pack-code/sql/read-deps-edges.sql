SELECT DISTINCT source_id, target_id
FROM graph_edges
WHERE relation = 'depends_on' AND deleted_at IS NULL AND target_backend IS NULL
  AND source_id IN (SELECT value FROM json_each(?1))
  AND target_id IN (SELECT value FROM json_each(?1))
ORDER BY source_id ASC, target_id ASC
