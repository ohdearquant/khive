UPDATE notes SET
properties = json_set(properties, '$.channel_slug', ?5,
                      '$.quarantine_content_ref', ?3),
expires_at = CASE WHEN ?6 IS NULL THEN expires_at
  WHEN expires_at IS NULL OR expires_at < ?6
  THEN ?6 ELSE expires_at END,
updated_at = MAX(updated_at, ?7)
WHERE id = ?1 AND namespace = ?2 AND kind = 'message'
  AND deleted_at IS NULL
  AND (json_type(properties, '$.quarantine_content_ref') IS NULL
       OR json_extract(properties, '$.quarantine_content_ref') = ?3)
  AND json_extract(properties, '$.channel_kind') = ?4
  AND (json_type(properties, '$.channel_slug') IS NULL
       OR (json_type(properties, '$.channel_slug') = 'text'
           AND json_extract(properties, '$.channel_slug') = ?5))
  AND (json_extract(properties, '$.quarantined') = 'true'
       OR json_type(properties, '$.quarantined') = 'true')
