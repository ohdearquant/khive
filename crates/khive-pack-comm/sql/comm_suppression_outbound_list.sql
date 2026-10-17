SELECT id, scope, kind, value, source, created_by, created_at, reason
FROM comm_suppression
WHERE scope = 'outbound'
ORDER BY created_at DESC, id ASC
