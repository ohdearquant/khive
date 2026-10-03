CREATE INDEX IF NOT EXISTS idx_git_notes_live_commit_sha
ON notes(namespace, kind, json_extract(properties,'$.sha'))
WHERE kind='commit' AND deleted_at IS NULL
