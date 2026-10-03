SELECT id FROM notes WHERE kind=?1 AND namespace=?2
AND kind IN ('issue','pull_request')
AND deleted_at IS NULL AND json_extract(properties,'$.number')=?3
AND json_extract(properties,'$.project_id')=?4 LIMIT 1
