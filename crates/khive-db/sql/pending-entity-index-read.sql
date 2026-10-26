SELECT entity_id, namespace, source_version, generation, first_pending_at, targets_json
FROM pending_entity_indexes
WHERE entity_id = ?1;
