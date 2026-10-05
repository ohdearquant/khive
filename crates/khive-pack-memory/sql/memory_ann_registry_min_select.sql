SELECT MIN(watermark) AS m FROM ann_consumer_watermark
WHERE (namespace = ?1 OR namespace = '*') AND embedding_model = ?2
