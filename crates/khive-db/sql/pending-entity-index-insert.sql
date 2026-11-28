INSERT INTO pending_entity_indexes
    (entity_id, namespace, source_version, generation, first_pending_at, targets_json)
VALUES (?1, ?2, ?3, ?4, ?5, ?6);
