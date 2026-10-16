UPDATE charter_phases
SET state = ?6, revision = revision + 1, updated_at_us = ?7
WHERE policy_domain = ?1 AND run_id = ?2 AND phase_id = ?3 AND revision = ?4 AND state = ?5
