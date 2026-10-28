SELECT CASE WHEN COUNT(*) = 1 AND MIN(current_head.id) = ?4 THEN 1 ELSE 0 END
FROM (
    SELECT document.id
    FROM entities AS document
    WHERE document.namespace = ?1
      AND document.kind = 'document'
      AND document.deleted_at IS NULL
      AND json_extract(document.properties, ?2) = ?3
      AND NOT EXISTS (
          SELECT 1
          FROM graph_edges AS edge
          WHERE edge.namespace = ?1
            AND edge.target_id = document.id
            AND edge.relation = ?5
            AND edge.deleted_at IS NULL
      )
    LIMIT 2
) AS current_head
