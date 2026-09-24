UPDATE notes
SET created_at = CASE WHEN ?8 = 1 THEN ?9 ELSE created_at END,
    updated_at = CASE WHEN ?10 = 1 THEN ?11 ELSE updated_at END,
    properties = json_set(
        CASE WHEN ?12 = 1
             THEN json_set(COALESCE(properties, '{}'), '$.status', ?13)
             ELSE COALESCE(properties, '{}')
        END,
        '$.gtd_repair', json(?14)
    )
WHERE id = ?1
  AND kind = 'task'
  AND deleted_at IS NULL
  AND properties IS ?2
  AND version = ?3
  AND created_at IS ?4
  AND updated_at IS ?5
  AND typeof(created_at) = ?6
  AND typeof(updated_at) = ?7
