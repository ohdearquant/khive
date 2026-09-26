SELECT id, deleted_at FROM entities WHERE id IN (SELECT value FROM json_each(?1))
UNION ALL
SELECT id, deleted_at FROM notes WHERE id IN (SELECT value FROM json_each(?1))
