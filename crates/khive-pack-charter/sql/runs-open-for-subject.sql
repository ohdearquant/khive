SELECT run_id, charter_id FROM charter_runs
WHERE policy_domain = ?1 AND subject_id = ?2 AND state = 'open' ORDER BY created_at_us, run_id
