SELECT * FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL ORDER BY created_at ASC, id ASC LIMIT ?2 OFFSET ?3
