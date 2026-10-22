SELECT sequence, kind, producer, payload_bytes, received_at_us FROM charter_evidence
WHERE policy_domain = ?1 AND run_id = ?2 AND sequence > ?3 ORDER BY sequence
