SELECT id, deleted_at FROM entities WHERE kind='project' AND namespace=?1
AND deleted_at IS NOT NULL
AND (json_extract(properties,'$.repo_slug')=?2
     OR json_extract(properties,'$.repo_url')=?3)
