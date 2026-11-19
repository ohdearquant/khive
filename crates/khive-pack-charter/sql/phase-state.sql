UPDATE charter_phases SET state = ?4, waiting_on = ?5, revision = revision + 1, updated_at_us = ?6
WHERE policy_domain = ?1 AND run_id = ?2 AND phase_id = ?3
