SELECT name, version, source_digest, source_bytes FROM charter_vocabularies
WHERE policy_domain = ?1 ORDER BY name, version
