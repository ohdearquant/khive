CREATE TABLE IF NOT EXISTS comm_suppression_mirror (
    mirror TEXT PRIMARY KEY CHECK (mirror = 'one_click'),
    refreshed_at INTEGER NOT NULL,
    remote_as_of TEXT NOT NULL CHECK (length(remote_as_of) > 0),
    remote_count INTEGER NOT NULL CHECK (remote_count >= 0)
)
