SELECT id FROM notes WHERE kind='commit' AND namespace=?1
AND deleted_at IS NULL AND json_extract(properties,'$.sha')=?2 LIMIT 1
