SELECT id FROM entities WHERE kind='document' AND namespace=?1
AND deleted_at IS NULL
AND (json_extract(properties,'$.source_uri')=?2 OR name=?3
     OR json_extract(properties,'$.source_uri') LIKE ?4 ESCAPE '\')
ORDER BY CASE WHEN json_extract(properties,'$.source_uri')=?2 OR name=?3
              THEN 0 ELSE 1 END, id
LIMIT 1
