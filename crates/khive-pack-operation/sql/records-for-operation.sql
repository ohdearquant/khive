SELECT sequence, kind, caller_key, payload_digest, payload_bytes, observed_at_us, recorded_at_us
FROM operation_records
WHERE operation_id = ?1
ORDER BY sequence
