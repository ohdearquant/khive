CREATE TABLE IF NOT EXISTS gtd_lifecycle_audit (
    note_id    TEXT NOT NULL,
    from_state TEXT NOT NULL,
    to_state   TEXT NOT NULL,
    note       TEXT,
    at         INTEGER NOT NULL,
    namespace  TEXT
)
