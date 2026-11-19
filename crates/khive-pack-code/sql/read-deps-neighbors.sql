WITH candidates(id) AS (
    SELECT target_id
    FROM graph_edges
    WHERE source_id IN (SELECT value FROM json_each(?1))
      AND relation = 'depends_on' AND deleted_at IS NULL AND target_backend IS NULL
      AND ?3 IN ('outgoing', 'both')
    UNION
    SELECT source_id
    FROM graph_edges
    WHERE target_id IN (SELECT value FROM json_each(?1))
      AND relation = 'depends_on' AND deleted_at IS NULL AND target_backend IS NULL
      AND ?3 IN ('incoming', 'both')
)
SELECT entities.id,
       CASE WHEN kind = 'project' THEN 'project' ELSE entity_type END AS projected_kind,
       name
FROM entities
JOIN candidates ON candidates.id = entities.id
WHERE deleted_at IS NULL
  AND (kind = 'project'
       OR (kind = 'concept' AND entity_type IN ('module', 'function', 'datatype', 'interface')))
  AND entities.id NOT IN (SELECT value FROM json_each(?2))
ORDER BY entities.id ASC
LIMIT ?4
