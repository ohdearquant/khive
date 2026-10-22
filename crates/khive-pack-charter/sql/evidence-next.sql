SELECT COALESCE(MAX(sequence), 0) + 1 AS next FROM charter_evidence
WHERE policy_domain = ?1 AND run_id = ?2
