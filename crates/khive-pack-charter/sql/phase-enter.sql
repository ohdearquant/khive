UPDATE charter_phases
SET state = 'waiting_trigger', round = round + 1, entered_sequence = ?4, waiting_on = NULL, revision = revision + 1, updated_at_us = ?5
WHERE policy_domain = ?1 AND run_id = ?2 AND phase_id = ?3
