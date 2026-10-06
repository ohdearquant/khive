INSERT OR IGNORE INTO ann_consumer_pending
(consumer, namespace, embedding_model, registered_at_us)
SELECT ?1, ?2, ?3, ?4
WHERE EXISTS (SELECT 1 FROM ann_consumer_watermark
WHERE consumer = ?1 AND namespace = ?2
AND embedding_model = ?3 AND watermark = ?5)
