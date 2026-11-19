SELECT d.source_bytes FROM charter_runs r
JOIN charter_definitions d
    ON d.policy_domain = r.policy_domain AND d.charter_id = r.charter_id
    AND d.version = r.definition_version AND d.definition_digest = r.definition_digest
WHERE r.run_id = ?1
