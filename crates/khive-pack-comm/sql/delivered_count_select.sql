SELECT COUNT(*) AS inbound_count
FROM notes
WHERE namespace = ?1
  AND kind = 'message'
  AND deleted_at IS NULL
  AND json_extract(properties, '$.direction') = 'inbound'
  AND json_extract(properties, '$.from_actor') = ?2
  AND json_extract(properties, '$.outbound_ref') = ?3
