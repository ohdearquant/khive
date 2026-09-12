SELECT DISTINCT json_extract(properties, '$.thread_id') AS thread_id
FROM notes
WHERE namespace IN (SELECT value FROM json_each(?1))
AND deleted_at IS NULL
AND json_type(properties, '$.thread_id') = 'text'
AND kind = 'message'
