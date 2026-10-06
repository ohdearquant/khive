SELECT id, namespace, verb, substrate, actor, kind, outcome, payload,
       payload_schema_version, profile_state_version, duration_us, target_id,
       session_id, aggregate_kind, aggregate_id, created_at, op_index, ref_resolution
FROM events
WHERE namespace = ?1
  AND created_at >= ?2
  AND created_at < ?3
  AND (json_array_length(?4) = 0 OR kind IN (SELECT value FROM json_each(?4)))
  AND (json_array_length(?5) = 0 OR actor IN (SELECT value FROM json_each(?5)))
  AND namespace NOT IN (SELECT value FROM json_each(?6))
  AND (?7 IS NULL OR (created_at, id COLLATE BINARY) > (?7, ?8))
ORDER BY created_at ASC, id COLLATE BINARY ASC
LIMIT ?9
