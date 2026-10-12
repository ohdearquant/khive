SELECT json_extract(properties, '$.to_actor') AS to_actor
FROM notes
WHERE kind = 'message'
  AND created_at >= ?1
  AND created_at <= ?2
  AND json_type(properties, '$.direction') = 'text'
  AND json_extract(properties, '$.direction') = 'outbound'
  AND json_type(properties, '$.to_actor') = 'text'
  AND substr(json_extract(properties, '$.to_actor'), 1, 6) = 'email:'
ORDER BY created_at ASC, id ASC
