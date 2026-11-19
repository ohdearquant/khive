SELECT id, deleted_at, tags FROM knowledge_atoms WHERE namespace = ?1 AND slug = ?2 LIMIT 1
