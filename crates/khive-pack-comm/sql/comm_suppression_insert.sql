INSERT INTO comm_suppression(id, scope, kind, value, source, created_by, created_at, reason)
VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
ON CONFLICT(scope, kind, value, source) DO NOTHING
