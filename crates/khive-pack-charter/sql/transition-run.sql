UPDATE charter_runs
SET state = CASE WHEN ?5 = 'terminated' THEN 'failed' ELSE ?5 END,
    terminal_outcome = CASE WHEN ?5 = 'terminated' THEN 'terminated' ELSE NULL END,
    revision = revision + 1, updated_at_us = ?6
WHERE policy_domain = ?1 AND run_id = ?2 AND revision = ?3
    AND COALESCE(terminal_outcome, state) = ?4
