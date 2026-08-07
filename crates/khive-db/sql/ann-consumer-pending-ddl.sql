-- Lifecycle metadata for ANN consumers which registered before their first
-- durable checkpoint.  `ann_consumer_watermark.watermark = -2` is the closed
-- pending value; the timestamp gives compaction a bounded, auditable grace
-- period before that never-activated registration is retired.
CREATE TABLE IF NOT EXISTS ann_consumer_pending (
    consumer         TEXT NOT NULL,
    namespace        TEXT NOT NULL,
    embedding_model  TEXT NOT NULL,
    registered_at_us INTEGER NOT NULL,
    PRIMARY KEY (consumer, namespace, embedding_model)
);

CREATE INDEX IF NOT EXISTS idx_ann_consumer_pending_registered
    ON ann_consumer_pending (registered_at_us);
