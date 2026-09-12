SELECT count(*) AS cnt FROM notes WHERE namespace = ?1 AND deleted_at IS NULL
