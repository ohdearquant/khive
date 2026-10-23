SELECT run_id FROM charter_runs
WHERE policy_domain = ?1 AND parent_run_id = ?2 AND state = 'open' ORDER BY created_at_us, run_id
