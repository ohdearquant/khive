SELECT request_digest, result_bytes, verb, run_id
FROM charter_commands
WHERE caller = ?1 AND (
    (?3 = 0 AND request_id = ?2)
    OR (?3 = 1 AND run_id = ?2 AND verb = 'charter.start')
)
    AND (?4 IS NULL OR policy_domain = ?4)
    AND (?5 IS NULL OR namespace IN (SELECT value FROM json_each(?5)))
ORDER BY created_at_us DESC
LIMIT 1
