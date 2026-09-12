SELECT id, json_extract(properties,'$.source_path') AS source_path
FROM entities WHERE kind='concept' AND entity_type='module'
AND namespace=?1 AND deleted_at IS NULL
AND json_type(properties,'$.source_path')='text'
AND json_extract(properties,'$.source_revision')=?2
ORDER BY source_path, id
