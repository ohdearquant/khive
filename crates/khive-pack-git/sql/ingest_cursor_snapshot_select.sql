SELECT kind, updated_at, typeof(cursor_value) AS value_type,
length(CAST(cursor_value AS BLOB)) AS value_bytes,
CASE WHEN length(CAST(cursor_value AS BLOB)) <= ?4 THEN CAST(cursor_value AS BLOB) END AS value
FROM git_mirror_cursor WHERE project_id=?1 AND kind IN (?2, ?3)
