DELETE FROM ann_write_log
WHERE namespace = ?1 AND embedding_model = ?2
AND seq <= (SELECT MIN(watermark)
FROM ann_consumer_watermark
WHERE (namespace = ?1 OR namespace = '*')
AND embedding_model = ?2)
