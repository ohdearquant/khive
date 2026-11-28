SELECT sequence, kind, payload_digest
FROM operation_records
WHERE operation_id = ?1 AND caller_key = ?2
