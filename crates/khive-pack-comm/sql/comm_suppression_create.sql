CREATE TABLE IF NOT EXISTS comm_suppression (
    id TEXT PRIMARY KEY CHECK (length(id) = 36),
    scope TEXT NOT NULL CHECK (scope IN ('outbound', 'inbound')),
    kind TEXT NOT NULL CHECK (kind IN ('address', 'domain', 'recipient_key')),
    value TEXT NOT NULL CHECK (length(value) > 0),
    source TEXT NOT NULL CHECK (source IN ('operator', 'mailto_token', 'one_click', 'reply_stop')),
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    reason TEXT NOT NULL,
    UNIQUE (scope, kind, value, source)
)
