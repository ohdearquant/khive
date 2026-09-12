SELECT DISTINCT namespace FROM entities WHERE deleted_at IS NULL
UNION
SELECT DISTINCT namespace FROM notes WHERE deleted_at IS NULL
