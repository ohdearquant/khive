SELECT slug, source_uri, properties FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL
