SELECT c.event_id,c.epoch_id,c.action_id,c.record_kind,c.record_id,
       e.id,e.namespace,e.verb,e.substrate,e.actor,e.kind,e.outcome,e.payload,
       e.payload_schema_version,e.profile_state_version,e.duration_us,
       e.target_id,e.session_id,e.aggregate_kind,e.aggregate_id,e.created_at,
       e.op_index,e.ref_resolution
FROM authority_changes AS c
JOIN events AS e ON e.id=c.event_id
WHERE c.epoch_id=?1 AND c.event_id=?2;
