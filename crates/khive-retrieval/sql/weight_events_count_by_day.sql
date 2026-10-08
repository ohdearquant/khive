SELECT ts / 86400000000 AS day_bucket, COUNT(*) as cnt
FROM weight_events
WHERE namespace = ?1 AND ts >= ?2
GROUP BY day_bucket
ORDER BY day_bucket ASC
