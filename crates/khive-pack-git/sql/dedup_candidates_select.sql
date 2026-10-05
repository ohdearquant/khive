WITH selected AS MATERIALIZED (
    SELECT n.id, n.kind, n.version, n.created_at,
           COALESCE(length(CAST(n.name AS BLOB)), 0)
             + length(CAST(n.content AS BLOB))
             + COALESCE(length(CAST(n.properties AS BLOB)), 0) AS payload_bytes,
           EXISTS(SELECT 1 FROM graph_edges e WHERE e.source_id = n.id AND e.target_id = ?2
                  AND e.namespace = ?1 AND e.relation = 'annotates'
                  AND e.deleted_at IS NULL AND e.target_backend IS NULL) AS canonical_annotation,
           EXISTS(SELECT 1 FROM graph_edges e JOIN entities p ON p.id = e.target_id
                  WHERE e.source_id = n.id AND e.relation = 'annotates'
                    AND e.deleted_at IS NULL AND e.target_backend IS NULL
                    AND p.kind = 'project' AND p.deleted_at IS NULL AND p.id != ?2)
                  AS other_project_annotation
    FROM notes n
    WHERE n.namespace = ?1 AND n.kind IN ('issue', 'pull_request') AND n.deleted_at IS NULL
      AND (lower(replace(CASE WHEN json_type(n.properties, '$.project_id') = 'text'
                              AND length(CAST(json_extract(n.properties, '$.project_id') AS BLOB)) <= 64
                         THEN json_extract(n.properties, '$.project_id') END, '-', ''))
           IN (SELECT value FROM json_each(?3))
           OR EXISTS(SELECT 1 FROM graph_edges e WHERE e.source_id = n.id AND e.target_id = ?2
                     AND e.relation = 'annotates' AND e.deleted_at IS NULL
                     AND e.target_backend IS NULL))
    ORDER BY n.id LIMIT ?4
), sized AS (
    SELECT *, SUM(payload_bytes) OVER (ORDER BY id) AS total_bytes FROM selected
)
SELECT CASE WHEN length(s.id) <= 64 THEN s.id END AS id, s.kind, s.version, s.created_at,
       payload_bytes <= ?5 AND total_bytes <= ?6 AS within_budget,
       CASE WHEN payload_bytes <= ?5 AND total_bytes <= ?6 THEN n.name END AS name,
       CASE WHEN payload_bytes <= ?5 AND total_bytes <= ?6 THEN n.content END AS content,
       CASE WHEN payload_bytes <= ?5 AND total_bytes <= ?6 THEN n.properties END AS properties,
       canonical_annotation, other_project_annotation
FROM sized s JOIN notes n ON n.id = s.id ORDER BY s.id
