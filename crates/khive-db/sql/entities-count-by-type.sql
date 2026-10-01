SELECT entity_type, COUNT(*) AS count
FROM entities
WHERE deleted_at IS NULL
  AND namespace IN (SELECT value FROM json_each(?1))
GROUP BY entity_type COLLATE BINARY
