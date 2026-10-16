WITH ranked AS (
    SELECT json_extract(properties,'$.sha') AS sha, id, deleted_at,
           row_number() OVER (
               PARTITION BY json_extract(properties,'$.sha') COLLATE BINARY
               ORDER BY id
           ) AS position
    FROM notes WHERE namespace=?1 AND kind='commit'
    AND json_type(properties,'$.sha')='text'
    AND json_extract(properties,'$.sha') COLLATE BINARY IN (SELECT value FROM json_each(?2))
)
SELECT sha, id, deleted_at FROM ranked WHERE position<=3
ORDER BY sha COLLATE BINARY, id
