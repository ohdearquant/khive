SELECT json_extract(properties, '$.content_hash') AS content_hash,
       group_concat(id) AS ids,
       count(*) AS c
FROM entities
WHERE entity_type = 'module' AND deleted_at IS NULL
  AND json_extract(properties, '$.content_hash') IS NOT NULL
GROUP BY content_hash
HAVING c > 1
ORDER BY content_hash;
