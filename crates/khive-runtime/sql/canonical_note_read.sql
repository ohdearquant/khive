SELECT id, namespace, kind, status, name, content, salience, decay_factor,
       expires_at, properties, created_at, updated_at, deleted_at, key, version
FROM notes WHERE id = ?1
