SELECT id,kind,json_type(properties,'$.number'),
CASE WHEN json_type(properties,'$.number')='integer' THEN json_extract(properties,'$.number') END,
CASE WHEN json_type(properties,'$.project_id')='text'
AND length(CAST(json_extract(properties,'$.project_id') AS BLOB))<=64
THEN json_extract(properties,'$.project_id') ELSE NULL END
FROM notes WHERE namespace=?1 AND deleted_at IS NULL
AND kind IN ('issue','pull_request') AND json_extract(properties,'$.url')=?2
LIMIT 10001
