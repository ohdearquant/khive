SELECT id FROM notes
WHERE namespace = ?1 AND kind = 'message'
  AND ((expires_at IS NOT NULL AND expires_at <= ?2)
       OR (expires_at IS NULL AND created_at <= ?3))
  AND json_extract(properties, '$.channel_kind') = ?4
  AND (json_type(properties, '$.channel_slug') IS NULL
       OR json_type(properties, '$.channel_slug') = 'null'
       OR (json_type(properties, '$.channel_slug') = 'text'
           AND trim(json_extract(properties, '$.channel_slug')) = ''))
  AND (json_extract(properties, '$.quarantined') = 'true'
       OR json_type(properties, '$.quarantined') = 'true')
ORDER BY COALESCE(expires_at, created_at), id LIMIT 128
