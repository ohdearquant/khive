SELECT id, namespace, verb, substrate, actor, kind, outcome, payload,
       payload_schema_version, profile_state_version, duration_us, target_id,
       session_id, aggregate_kind, aggregate_id, created_at
FROM events
WHERE id = ?1
LIMIT 1
