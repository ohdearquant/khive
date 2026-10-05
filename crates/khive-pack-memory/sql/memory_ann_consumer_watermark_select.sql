SELECT watermark FROM ann_consumer_watermark
WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3
