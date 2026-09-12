SELECT id FROM entities WHERE kind='project' AND namespace=?1
AND deleted_at IS NULL
AND json_extract(properties,'$.repo_slug') IS NULL
AND json_extract(properties,'$.repo_url')=?2
ORDER BY created_at ASC, id ASC
