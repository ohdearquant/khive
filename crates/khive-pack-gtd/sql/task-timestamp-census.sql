WITH scoped AS (
    SELECT
        created_at,
        typeof(created_at) AS created_type,
        json_extract(properties, '$.archived_at') AS archived_at,
        json_type(properties, '$.archived_at') AS archived_type
    FROM notes
    WHERE kind = 'task'
      AND deleted_at IS NULL
      AND namespace IN (SELECT value FROM json_each(?1))
), measurements AS (
    SELECT 'created_at' AS field, created_type AS value_type, created_at AS value
    FROM scoped
    UNION ALL
    SELECT 'archived_at', archived_type, archived_at
    FROM scoped
), bucketed AS (
    SELECT field,
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
)
SELECT field, bucket, COUNT(*) AS count
FROM bucketed
GROUP BY field, bucket
UNION ALL
SELECT 'comparison', 'created_at_gt_archived_at_raw', COUNT(*)
FROM scoped
WHERE created_type IN ('integer', 'real')
  AND archived_type IN ('integer', 'real')
  AND created_at > archived_at
