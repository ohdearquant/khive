SELECT DISTINCT json_extract(properties, '$.thread_id') AS thread_id
FROM notes
WHERE namespace IN (SELECT value FROM json_each(?1))
AND deleted_at IS NULL
AND json_type(properties, '$.thread_id') = 'text'
AND kind = 'message'
AND (
    (json_extract(properties, '$.direction') = 'inbound'
        AND ((json_type(properties, '$.to_actor') = 'text' AND json_extract(properties, '$.to_actor') = ?2)
            OR (?3 = 1 AND (json_type(properties, '$.to_actor') IS NULL
                OR json_type(properties, '$.to_actor') = 'null'))))
    OR (json_extract(properties, '$.direction') = 'outbound'
        AND ((json_type(properties, '$.from_actor') = 'text' AND json_extract(properties, '$.from_actor') = ?2)
            OR (?3 = 1 AND (json_type(properties, '$.from_actor') IS NULL
                OR json_type(properties, '$.from_actor') = 'null'))))
    OR (?3 = 1 AND (json_type(properties, '$.direction') IS NULL
            OR json_type(properties, '$.direction') = 'null')
        AND (json_type(properties, '$.from_actor') IS NULL
            OR json_type(properties, '$.from_actor') = 'null')
        AND (json_type(properties, '$.to_actor') IS NULL
            OR json_type(properties, '$.to_actor') = 'null'))
)
