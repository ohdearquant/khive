SELECT id, json_extract(properties,'$.repo_url') AS repo_url
FROM entities WHERE kind='project' AND namespace=?1
AND deleted_at IS NULL
AND json_extract(properties,'$.repo_url') IS NOT NULL
AND (json_extract(properties,'$.repo_slug') IS NULL
     OR json_extract(properties,'$.repo_slug')<>?2)
ORDER BY created_at ASC, id ASC
