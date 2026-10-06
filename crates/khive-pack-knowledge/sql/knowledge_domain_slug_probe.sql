SELECT id, deleted_at FROM knowledge_domains WHERE namespace = ?1 AND slug = ?2 LIMIT 1
