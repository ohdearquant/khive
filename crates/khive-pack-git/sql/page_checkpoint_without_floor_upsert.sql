INSERT INTO git_mirror_cursor(project_id, kind, cursor_value, updated_at)
VALUES(?1, ?2, ?3, ?4)
ON CONFLICT(project_id, kind) DO UPDATE SET
cursor_value=excluded.cursor_value, updated_at=excluded.updated_at
