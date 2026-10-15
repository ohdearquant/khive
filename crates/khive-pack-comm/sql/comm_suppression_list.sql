SELECT id, scope, kind, value, source, created_by, created_at, reason
FROM comm_suppression
WHERE (?1 IS NULL OR scope = ?1)
  AND (?2 IS NULL OR kind = ?2)
ORDER BY created_at DESC, id ASC
LIMIT ?3 OFFSET ?4
