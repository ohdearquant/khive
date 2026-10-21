SELECT request_digest, result_bytes
FROM charter_commands
WHERE caller = ?1 AND request_id = ?2
