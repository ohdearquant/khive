SELECT EXISTS(SELECT 1 FROM entities WHERE id=?1 AND namespace=?2
              AND kind='project' AND deleted_at IS NULL
              AND json_type(properties,'$.repo_slug')='text'
              AND json_extract(properties,'$.repo_slug')=?3 COLLATE BINARY)
