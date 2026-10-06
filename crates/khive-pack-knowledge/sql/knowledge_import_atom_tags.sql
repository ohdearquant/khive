SELECT tags, deleted_at FROM knowledge_atoms WHERE slug = ?1 AND namespace = ?2 LIMIT 1
