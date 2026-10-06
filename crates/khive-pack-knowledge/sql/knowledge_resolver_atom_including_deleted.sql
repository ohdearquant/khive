SELECT id, namespace, slug, name, content, tags, properties, status, finalized, source_uri, source_type, created_at, updated_at, deleted_at FROM knowledge_atoms WHERE id = ?1 LIMIT 1
