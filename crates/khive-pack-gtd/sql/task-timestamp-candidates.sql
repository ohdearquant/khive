WITH scoped AS (
    SELECT
        id,
        namespace,
        created_at,
        typeof(created_at) AS created_type,
        updated_at,
        typeof(updated_at) AS updated_type,
        properties -> '$.status' AS stored_status_json,
        properties -> '$.archived_at' AS archived_json,
        json_extract(properties, '$.archived_at') AS archived_at,
        json_type(properties, '$.archived_at') AS archived_type
    FROM notes
    WHERE kind = 'task'
      AND deleted_at IS NULL
      AND namespace IN (SELECT value FROM json_each(?1))
      AND (?2 IS NULL
           OR namespace COLLATE BINARY > ?2
           OR (namespace = ?2 AND id COLLATE BINARY > ?3))
), measurements AS (
    SELECT id, namespace, 'created_at' AS field, created_type AS value_type, created_at AS value
    FROM scoped
    UNION ALL
    SELECT id, namespace, 'updated_at', updated_type, updated_at
    FROM scoped
    UNION ALL
    SELECT id, namespace, 'archived_at', archived_type, archived_at
    FROM scoped
), bucketed AS (
    SELECT id, namespace, field,
        CASE
            WHEN value_type IS NULL OR value_type = 'null' THEN 'null'
            WHEN value_type NOT IN ('integer', 'real') THEN 'nonnumeric'
            WHEN value = 0 THEN 'epoch_zero'
            WHEN (value >= 1000000000 AND value < 10000000000)
              OR (value > -10000000000 AND value <= -1000000000)
                THEN 'magnitude_10_digits'
            WHEN (value >= 1000000000000 AND value < 10000000000000)
              OR (value > -10000000000000 AND value <= -1000000000000)
                THEN 'magnitude_13_digits'
            WHEN (value >= 1000000000000000 AND value < 10000000000000000)
              OR (value > -10000000000000000 AND value <= -1000000000000000)
                THEN 'magnitude_16_digits'
            ELSE 'other'
        END AS bucket
    FROM measurements
), classified AS (
    SELECT id, namespace,
        MAX(CASE WHEN field = 'created_at' THEN bucket END) AS created_bucket,
        MAX(CASE WHEN field = 'updated_at' THEN bucket END) AS updated_bucket,
        MAX(CASE WHEN field = 'archived_at' THEN bucket END) AS archived_bucket
    FROM bucketed
    GROUP BY namespace, id
)
SELECT s.id, s.namespace, s.created_at, s.updated_at, s.stored_status_json, s.archived_json,
       c.created_bucket, c.updated_bucket, c.archived_bucket
FROM scoped s
JOIN classified c ON c.namespace = s.namespace AND c.id = s.id
WHERE c.created_bucket != 'magnitude_16_digits'
   OR c.updated_bucket != 'magnitude_16_digits'
ORDER BY s.namespace COLLATE BINARY, s.id COLLATE BINARY
LIMIT ?4
