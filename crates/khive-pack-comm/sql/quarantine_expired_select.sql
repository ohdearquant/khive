SELECT id FROM notes
WHERE namespace = ?1 AND kind = 'message' AND deleted_at IS NULL
  AND expires_at IS NOT NULL AND expires_at <= ?2
  AND json_extract(properties, '$.channel_kind') = ?3
  AND json_extract(properties, '$.channel_slug') = ?4
  AND (json_extract(properties, '$.quarantined') = 'true'
       OR json_type(properties, '$.quarantined') = 'true')
ORDER BY expires_at, id LIMIT 128
