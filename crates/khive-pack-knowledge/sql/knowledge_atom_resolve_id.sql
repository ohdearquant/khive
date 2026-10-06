SELECT id FROM knowledge_atoms WHERE id = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1
