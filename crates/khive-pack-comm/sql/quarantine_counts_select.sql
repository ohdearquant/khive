SELECT json_extract(properties, '$.channel_kind') AS channel_kind,
       json_extract(properties, '$.channel_slug') AS channel_slug,
       COUNT(*) AS quarantined_count
FROM notes
WHERE namespace = ?1
  AND kind = 'message'
  AND deleted_at IS NULL
  AND (json_extract(properties, '$.quarantined') = 'true'
       OR json_type(properties, '$.quarantined') = 'true')
GROUP BY json_extract(properties, '$.channel_kind'),
         json_extract(properties, '$.channel_slug')
ORDER BY channel_kind, channel_slug
