INSERT INTO ann_consumer_pending
(consumer, namespace, embedding_model, registered_at_us)
VALUES (?1, ?2, ?3, ?4)
ON CONFLICT(consumer, namespace, embedding_model)
DO UPDATE SET registered_at_us = excluded.registered_at_us
