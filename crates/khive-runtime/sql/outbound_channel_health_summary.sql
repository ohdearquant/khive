WITH eligible AS (
    SELECT id, created_at,
           json_extract(properties, '$.next_attempt_at') AS next_attempt_at,
           khive_rfc3339_strict_key(json_extract(properties, '$.next_attempt_at')) AS retry_key,
           -- Raw JSON text preserves unsigned integers above SQLite's signed range.
           -- Equal-width decimal keys preserve numeric ordering without a floating conversion.
           CASE
               WHEN json_type(properties, '$.delivery_attempts') = 'integer'
                    AND substr(properties -> '$.delivery_attempts', 1, 1) != '-'
                    AND (length(properties -> '$.delivery_attempts') < 20
                         OR (length(properties -> '$.delivery_attempts') = 20
                             AND (properties -> '$.delivery_attempts') <= '18446744073709551615'))
               THEN substr('00000000000000000000' || (properties -> '$.delivery_attempts'), -20)
               ELSE '00000000000000000000'
           END AS attempts_key
    FROM notes
    WHERE namespace = ?1 AND kind = 'message' AND deleted_at IS NULL
      AND json_extract(properties, '$.direction') = 'outbound'
      AND substr(json_extract(properties, '$.to_actor'), 1,
                 instr(json_extract(properties, '$.to_actor'), ':')) = ?2
      AND ifnull(json_type(properties, '$.delivered_at'), 'null') = 'null'
      AND (json_extract(properties, '$.delivery') IS NULL
           OR json_extract(properties, '$.delivery') NOT IN ('delivered', 'failed'))
      AND ifnull(json_type(properties, '$.delivery_hold'), 'null') = 'null'
      AND (json_extract(properties, '$.channel_slug') = ?3
           OR (?4 = 1 AND json_type(properties, '$.channel_slug') IS NULL))
), classified AS (
    SELECT *, ifnull(retry_key > khive_rfc3339_strict_key(?5), 0) AS in_backoff
    FROM eligible
)
SELECT count(*) AS undelivered_rows,
       coalesce(sum(in_backoff), 0) AS backoff_rows,
       min(created_at) AS oldest_undelivered_created_at,
       min(CASE WHEN in_backoff = 1 THEN created_at END) AS oldest_backoff_created_at,
       max(CASE WHEN in_backoff = 1 THEN attempts_key END) AS max_backoff_delivery_attempts,
       (SELECT next_attempt_at FROM classified WHERE in_backoff = 1
        ORDER BY retry_key, next_attempt_at, id LIMIT 1) AS earliest_backoff_next_attempt_at
FROM classified;
