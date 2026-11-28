-- Read through runtime.sql(), the same assigned backend used by the write fence.
-- A single statement gives history and live supersedes edges one observation.
SELECT EXISTS(
    SELECT 1 FROM entities AS history
    WHERE history.namespace = ?1 AND history.kind = 'document'
      AND history.deleted_at IS NULL
      AND json_extract(history.properties, ?2) = ?3
) AS has_history, COUNT(*) AS head_count, MIN(id) AS head_id
FROM (
    SELECT document.id FROM entities AS document
    WHERE document.namespace = ?1 AND document.kind = 'document'
      AND document.deleted_at IS NULL
      AND json_extract(document.properties, ?2) = ?3
      AND NOT EXISTS (
          SELECT 1 FROM graph_edges AS edge
          WHERE edge.namespace = ?1 AND edge.target_id = document.id
            AND edge.relation = ?4 AND edge.deleted_at IS NULL
      )
    LIMIT 2
) AS heads;
