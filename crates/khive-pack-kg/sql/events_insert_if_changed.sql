INSERT INTO events
       (id, namespace, verb, substrate, actor, kind, outcome, payload,
        payload_schema_version, profile_state_version, duration_us,
        target_id, session_id, aggregate_kind, aggregate_id, created_at)
SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16
WHERE (changes() = 1)
