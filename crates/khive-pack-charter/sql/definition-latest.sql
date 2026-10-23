SELECT charter_id, version, definition_digest, definition_bytes, namespace FROM charter_definitions
WHERE policy_domain = ?1 AND charter_id = ?2 ORDER BY version DESC LIMIT 1
