DELETE FROM ann_consumer_pending
WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3
