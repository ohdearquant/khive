CREATE TABLE IF NOT EXISTS git_mirror_cursor (
    project_id   TEXT NOT NULL,
    kind         TEXT NOT NULL,
    cursor_value TEXT,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (project_id, kind)
)
