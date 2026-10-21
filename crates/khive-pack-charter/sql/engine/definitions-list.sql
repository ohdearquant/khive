SELECT charter_id, version, definition_digest, gate_registry_digest, created_at_us FROM charter_definitions
WHERE policy_domain = ?1 ORDER BY charter_id, version
