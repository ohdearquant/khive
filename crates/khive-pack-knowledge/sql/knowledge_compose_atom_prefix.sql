SELECT * FROM knowledge_atoms WHERE id LIKE ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 2
