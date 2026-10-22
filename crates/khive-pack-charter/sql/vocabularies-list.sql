SELECT name, version, source_digest, source_bytes FROM charter_vocabularies
WHERE policy_domain = ?1 AND namespace IN (SELECT value FROM json_each(?2))
ORDER BY name, version
