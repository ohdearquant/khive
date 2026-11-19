SELECT
    s.id,
    s.scope,
    s.kind,
    s.value,
    s.source,
    s.created_by,
    s.created_at,
    s.reason,
    n.id AS evidence_id,
    n.created_at AS evidence_created_at,
    json_extract(n.properties, '$.direction') AS evidence_direction,
    json_extract(n.properties, '$.from') AS evidence_from,
    json_extract(n.properties, '$.channel_kind') AS evidence_channel_kind,
    json_extract(n.properties, '$.quarantined') AS evidence_quarantined,
    json_extract(n.properties, '$.authenticated') AS evidence_authenticated,
    json_extract(n.properties, '$.recipient_key') AS evidence_recipient_key
FROM comm_suppression AS s
LEFT JOIN notes AS n
    ON n.id = ?2 AND n.kind = 'message' AND n.deleted_at IS NULL
WHERE s.id = ?1
