UPDATE pending_entity_indexes
SET namespace = ?2, source_version = ?3, generation = ?4,
    first_pending_at = ?5, targets_json = ?6
WHERE entity_id = ?1 AND generation = ?7;
