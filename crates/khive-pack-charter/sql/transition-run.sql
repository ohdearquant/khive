UPDATE charter_runs
SET state = ?5, revision = revision + 1, updated_at_us = ?6
WHERE policy_domain = ?1 AND run_id = ?2 AND revision = ?3 AND state = ?4
