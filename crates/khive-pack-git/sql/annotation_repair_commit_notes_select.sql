SELECT id, deleted_at FROM notes WHERE namespace=?1
AND kind='commit' AND json_type(properties,'$.sha')='text'
AND json_extract(properties,'$.sha')=?2 COLLATE BINARY
ORDER BY id LIMIT 3
