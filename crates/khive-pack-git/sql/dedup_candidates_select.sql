SELECT n.id, n.kind, n.version, n.created_at, n.name, n.content, n.properties,
       EXISTS(SELECT 1 FROM graph_edges e
              WHERE e.source_id = n.id AND e.target_id = ?2 AND e.namespace = ?1
                AND e.relation = 'annotates' AND e.deleted_at IS NULL
                AND e.target_backend IS NULL) AS canonical_annotation,
       EXISTS(SELECT 1 FROM graph_edges e JOIN entities p ON p.id = e.target_id
              WHERE e.source_id = n.id AND e.namespace = ?1
                AND e.relation = 'annotates' AND e.deleted_at IS NULL
                AND e.target_backend IS NULL
                AND p.namespace = ?1 AND p.kind = 'project'
                AND p.deleted_at IS NULL AND p.merged_into IS NULL
                AND p.id != ?2) AS other_project_annotation
FROM notes n
WHERE n.namespace = ?1 AND n.kind IN ('issue', 'pull_request') AND n.deleted_at IS NULL
  AND (json_extract(n.properties, '$.project_id') IN (SELECT value FROM json_each(?3))
       OR EXISTS(SELECT 1 FROM graph_edges e
                 WHERE e.source_id = n.id AND e.target_id = ?2 AND e.namespace = ?1
                   AND e.relation = 'annotates' AND e.deleted_at IS NULL
                   AND e.target_backend IS NULL))
ORDER BY n.id
