DELETE FROM ann_write_log
WHERE embedding_model = ?1
AND seq <= (SELECT MIN(watermark.watermark)
FROM ann_consumer_watermark watermark
WHERE (watermark.namespace = ann_write_log.namespace
OR watermark.namespace = '*')
AND watermark.embedding_model = ?1)
