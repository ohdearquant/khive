SELECT id, scope, kind, value, source, created_by, created_at, reason
FROM comm_suppression
WHERE scope = 'inbound' AND kind IN ('address', 'domain')
ORDER BY created_at DESC, id ASC
