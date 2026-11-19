SELECT run_id FROM charter_runs
WHERE policy_domain = ?1 AND charter_id = ?2 AND subject_id = ?3 AND state = 'open' ORDER BY created_at_us, run_id
