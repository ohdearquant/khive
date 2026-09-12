SELECT COUNT(*) FROM entities WHERE namespace = ?1 AND deleted_at IS NULL AND entity_type IS NULL
