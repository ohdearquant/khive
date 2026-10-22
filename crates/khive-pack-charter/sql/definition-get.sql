SELECT charter_id, version, definition_digest, definition_bytes, namespace FROM charter_definitions
WHERE policy_domain = ?1 AND charter_id = ?2 AND version = ?3
    AND (?4 IS NULL OR namespace IN (SELECT value FROM json_each(?4)))
