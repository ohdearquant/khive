DELETE FROM ann_write_log
WHERE namespace = ?1 AND embedding_model = ?2
AND seq <= (SELECT MIN(watermark.watermark)
FROM ann_consumer_watermark watermark
WHERE (watermark.namespace = ?1
OR watermark.namespace = '*')
AND watermark.embedding_model = ?2)
