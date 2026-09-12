SELECT id, profile_id, event_kind, payload, created_at
FROM brain_event_log
WHERE namespace = ?1
AND created_at > ?2
ORDER BY created_at ASC, id ASC
