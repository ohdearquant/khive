INSERT INTO operation_records
    (operation_id, sequence, namespace, kind, caller_key, payload_digest, payload_bytes,
     observed_at_us, recorded_at_us, supersedes_sequence)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)
