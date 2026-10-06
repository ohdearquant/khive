WITH RECURSIVE lineage(id) AS (
    SELECT id FROM entities
    WHERE id = ?1 AND namespace = ?2 AND kind = 'project'
      AND deleted_at IS NULL AND merged_into IS NULL
    UNION
    SELECT e.id FROM entities e JOIN lineage l ON e.merged_into = l.id
    WHERE e.namespace = ?2 AND e.kind = 'project' AND e.deleted_at IS NOT NULL
)
SELECT id FROM lineage ORDER BY id
