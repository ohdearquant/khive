SELECT sequence, payload_digest FROM charter_evidence
WHERE policy_domain = ?1 AND run_id = ?2 AND producer = ?3 AND kind = ?4 AND source_identity = ?5 AND source_event_id = ?6
