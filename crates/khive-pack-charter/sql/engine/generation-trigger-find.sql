SELECT request_digest FROM charter_commands
WHERE policy_domain = ?1 AND caller = ?2 AND request_id = ?3
