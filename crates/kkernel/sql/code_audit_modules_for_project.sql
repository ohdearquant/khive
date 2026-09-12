SELECT id, properties FROM entities
WHERE entity_type = 'module' AND deleted_at IS NULL
  AND json_extract(properties,'$.source_project') = ?1
ORDER BY id;
