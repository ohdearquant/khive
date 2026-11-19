-- Project containment includes nested L2 declarations. UNION terminates cycles.
WITH RECURSIVE code AS (
    SELECT id, name, CASE WHEN kind = 'project' THEN 'project' ELSE entity_type END AS kind
    FROM entities
    WHERE deleted_at IS NULL AND (kind = 'project' OR
        (kind = 'concept' AND entity_type IN ('module', 'function', 'datatype', 'interface')))
), membership(project_id, id) AS (
    SELECT id, id FROM code WHERE kind = 'project'
    UNION
    SELECT membership.project_id, child.id
    FROM membership
    JOIN graph_edges edge ON edge.source_id = membership.id
    JOIN code child ON child.id = edge.target_id AND child.kind != 'project'
    WHERE edge.relation = 'contains' AND edge.deleted_at IS NULL
      AND edge.target_backend IS NULL
), projected AS (
    SELECT code.id, code.kind, code.name,
        CASE WHEN code.kind = 'project' THEN NULL ELSE (
            SELECT MIN(project_id) FROM membership WHERE membership.id = code.id
        ) END AS project_id
    FROM code
), filtered AS (
    SELECT * FROM projected
    WHERE (?1 IS NULL OR kind = ?1)
      AND (?2 IS NULL OR project_id = ?2)
)
SELECT id, kind, name, project_id FROM filtered
WHERE (?3 IS NULL OR (name, id) > (?3, ?4))
  AND (?6 IS NULL OR (name = ?5 AND id = ?6))
ORDER BY name ASC, id ASC LIMIT ?7
