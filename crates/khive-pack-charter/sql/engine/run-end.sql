UPDATE charter_runs SET state = ?3, revision = revision + 1, updated_at_us = ?4
WHERE policy_domain = ?1 AND run_id = ?2 AND state = 'open'
