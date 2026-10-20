SELECT 'entity' AS substrate, id, namespace, kind, entity_type,
       NULL AS status, version, updated_at, deleted_at
FROM entities WHERE id = ?1
UNION ALL
SELECT 'note', id, namespace, kind, NULL, status, version, updated_at, deleted_at
FROM notes WHERE id = ?1
UNION ALL
SELECT 'event', id, namespace, kind, NULL, NULL, NULL, created_at, NULL
FROM events WHERE id = ?1
UNION ALL
SELECT 'edge', id, namespace, relation, NULL, NULL, NULL, updated_at, deleted_at
FROM graph_edges WHERE id = ?1
ORDER BY substrate, namespace
