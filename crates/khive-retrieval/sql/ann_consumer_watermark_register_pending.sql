INSERT OR IGNORE INTO ann_consumer_watermark
(consumer, namespace, embedding_model, watermark)
VALUES (?1, ?2, ?3, ?4)
