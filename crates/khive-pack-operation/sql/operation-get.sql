SELECT operation_id, namespace, definition_namespace, definition_id, definition_version,
    definition_digest, parent_operation_id, opened_by, opened_at_us
FROM operations
WHERE operation_id = ?1
