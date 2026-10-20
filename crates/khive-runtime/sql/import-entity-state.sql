SELECT id, namespace, kind, entity_type, version, updated_at, deleted_at
FROM entities WHERE id = ?1 ORDER BY namespace
