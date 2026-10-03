CREATE INDEX IF NOT EXISTS idx_git_notes_live_number_project
ON notes(namespace, kind, json_extract(properties,'$.number'),
         json_extract(properties,'$.project_id'))
WHERE kind IN ('issue','pull_request') AND deleted_at IS NULL
