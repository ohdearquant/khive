SELECT sequence, kind, producer, source_event_id, payload_bytes, received_at_us FROM charter_evidence
WHERE policy_domain = ?1 AND run_id = ?2 ORDER BY sequence DESC LIMIT ?3
