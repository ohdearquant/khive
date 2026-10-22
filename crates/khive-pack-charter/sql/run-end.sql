UPDATE charter_runs
SET state = CASE WHEN ?3 = 'terminated' THEN 'failed' ELSE ?3 END,
    terminal_outcome = CASE WHEN ?3 = 'terminated' THEN 'terminated' ELSE NULL END,
    revision = revision + 1, updated_at_us = ?4
WHERE policy_domain = ?1 AND run_id = ?2 AND state = 'open'
