SELECT EXISTS (
    SELECT 1 FROM git_mirror_cursor
    WHERE project_id = ?1 AND kind = 'commits'
      AND typeof(cursor_value) = 'text'
      AND CAST(cursor_value AS BLOB) = ?2 AND updated_at = ?3
) AND EXISTS (
    SELECT 1 FROM git_mirror_cursor
    WHERE project_id = ?1 AND kind = 'commits_checkpoint'
      AND typeof(cursor_value) = 'text'
      AND CAST(cursor_value AS BLOB) = ?4 AND updated_at = ?5
)
