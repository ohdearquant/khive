SELECT namespace, definition_id, version, kind, digest, definition_bytes
FROM operation_definitions
WHERE definition_id = ?1 AND kind = 'operation_type'
    AND namespace IN (SELECT value FROM json_each(?2))
    AND (?3 IS NULL OR namespace = ?3)
    AND (?4 IS NULL OR version = ?4)
ORDER BY namespace, version DESC
