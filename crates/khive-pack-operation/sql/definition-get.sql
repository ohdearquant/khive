SELECT namespace, definition_id, version, kind, digest, definition_bytes
FROM operation_definitions
WHERE namespace = ?1 AND definition_id = ?2 AND version = ?3
