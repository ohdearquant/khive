SELECT sequence, payload_bytes FROM charter_evidence
WHERE policy_domain = ?1 AND run_id = ?2 AND kind = 'charter.entered' ORDER BY sequence
