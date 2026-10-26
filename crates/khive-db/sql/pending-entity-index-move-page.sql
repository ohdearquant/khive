SELECT pending.entity_id, pending.namespace, pending.source_version,
       pending.generation, pending.first_pending_at, pending.targets_json,
       entity.version AS live_version
FROM pending_entity_indexes AS pending
JOIN entities AS entity ON entity.id = pending.entity_id
WHERE entity.namespace = ?1 AND entity.kind = ?2
  AND (?3 IS NULL OR pending.entity_id > ?3)
ORDER BY pending.entity_id
LIMIT ?4;
