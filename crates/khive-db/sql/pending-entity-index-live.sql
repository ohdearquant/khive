SELECT id FROM entities
WHERE id = ?1 AND namespace = ?2 AND version = ?3
    AND deleted_at IS NULL AND merged_into IS NULL;
