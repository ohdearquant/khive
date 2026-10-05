WITH RECURSIVE lineage(id, depth) AS (
    SELECT id, 0 FROM entities
    WHERE id = ?1 AND namespace = ?2 AND kind = 'project'
      AND deleted_at IS NULL AND merged_into IS NULL
    UNION ALL
    SELECT e.id, l.depth + 1 FROM entities e JOIN lineage l ON e.merged_into = l.id
    WHERE e.namespace = ?2 AND e.kind = 'project' AND e.deleted_at IS NOT NULL
      AND l.depth < ?3
    LIMIT ?4
)
SELECT id, depth,
       EXISTS(SELECT 1 FROM entities e WHERE e.merged_into = lineage.id
              AND e.namespace = ?2 AND e.kind = 'project' AND e.deleted_at IS NOT NULL)
       AS has_children
FROM lineage ORDER BY depth, id
