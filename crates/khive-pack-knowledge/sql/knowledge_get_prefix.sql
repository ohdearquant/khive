SELECT id FROM knowledge_domains
WHERE id >= ?1 AND id < ?2 AND deleted_at IS NULL
UNION
SELECT id FROM knowledge_atoms
WHERE id >= ?1 AND id < ?2 AND deleted_at IS NULL
ORDER BY id LIMIT 2
