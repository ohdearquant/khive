SELECT *
FROM knowledge_atoms
WHERE namespace = ?1 AND deleted_at IS NULL
  AND (created_at, id) > (?3, ?4)
ORDER BY created_at ASC, id ASC
LIMIT ?2
