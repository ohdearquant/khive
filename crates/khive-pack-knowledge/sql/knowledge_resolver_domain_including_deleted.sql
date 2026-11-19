SELECT id, namespace, slug, name, description, tags, members, created_at, updated_at, deleted_at FROM knowledge_domains WHERE id = ?1 LIMIT 1
