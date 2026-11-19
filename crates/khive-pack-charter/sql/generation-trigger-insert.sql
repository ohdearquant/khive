INSERT INTO charter_commands (policy_domain, caller, request_id, namespace, verb, request_digest, disposition, result_bytes, created_at_us)
VALUES (?1, ?2, ?3, ?4, 'charter.observe', ?5, 'generation-event', ?6, ?7)
