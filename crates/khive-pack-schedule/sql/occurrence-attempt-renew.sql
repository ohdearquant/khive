UPDATE scheduled_attempts_v3 SET lease_expires_at_us=?5
WHERE occurrence_id=?1 AND invocation_id=?2 AND claim_generation=?3 AND owner_nonce=?4
AND NOT EXISTS (SELECT 1 FROM scheduled_receipts_v3 r
WHERE r.occurrence_id=?1 AND r.record_kind='terminal');
