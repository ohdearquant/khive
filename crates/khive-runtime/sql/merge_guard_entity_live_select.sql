SELECT EXISTS(SELECT 1 FROM entities WHERE id = ?1 AND namespace = ?2
AND deleted_at IS NULL AND merged_into IS NULL)
