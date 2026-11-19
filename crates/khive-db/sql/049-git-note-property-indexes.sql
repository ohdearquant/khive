CREATE INDEX IF NOT EXISTS idx_git_notes_live_commit_sha
ON notes(namespace, kind, json_extract(properties,'$.sha'))
WHERE kind='commit' AND deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_git_notes_live_number_project
ON notes(namespace, kind, json_extract(properties,'$.number'),
         json_extract(properties,'$.project_id'))
WHERE kind IN ('issue','pull_request') AND deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_git_notes_history_canonical_sha
ON notes(namespace, json_extract(properties, '$.sha'))
WHERE kind = 'commit'
  AND CASE WHEN json_valid(properties) = 1
           THEN json_type(properties, '$.sha') = 'text' ELSE 0 END;

CREATE INDEX IF NOT EXISTS idx_git_notes_history_noncanonical
ON notes(namespace, kind)
WHERE kind = 'commit' AND json_valid(properties) IS NOT 1;
