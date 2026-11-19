SELECT merged_into FROM entities WHERE id = ?1 AND namespace = ?2
AND deleted_at IS NOT NULL
