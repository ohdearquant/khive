UPDATE agents
SET state = ?1, terminal_reason = ?2, state_changed_at = ?3
WHERE agent_id = ?4 AND state = ?5;
