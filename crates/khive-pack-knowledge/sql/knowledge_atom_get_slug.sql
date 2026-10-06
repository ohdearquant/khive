SELECT * FROM knowledge_atoms WHERE namespace = ?1 AND slug = ?2 AND deleted_at IS NULL LIMIT 1
