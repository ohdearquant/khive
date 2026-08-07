-- Issue #1479: distinguish a registration which has never published a
-- checkpoint from an active consumer whose valid checkpoint happens to be at
-- sequence zero. Existing zero rows are ambiguous, so upgrade them to the
-- closed pending state with a fresh grace window. A live consumer will either
-- publish a checkpoint (including S=0) or enter its registry-loss rebuild path
-- before the row can be retired.
CREATE TABLE IF NOT EXISTS ann_consumer_pending (
    consumer         TEXT NOT NULL,
    namespace        TEXT NOT NULL,
    embedding_model  TEXT NOT NULL,
    registered_at_us INTEGER NOT NULL,
    PRIMARY KEY (consumer, namespace, embedding_model)
);

CREATE INDEX IF NOT EXISTS idx_ann_consumer_pending_registered
    ON ann_consumer_pending (registered_at_us);

INSERT OR IGNORE INTO ann_consumer_pending
    (consumer, namespace, embedding_model, registered_at_us)
SELECT consumer, namespace, embedding_model,
       CAST(strftime('%s', 'now') AS INTEGER) * 1000000
FROM ann_consumer_watermark
WHERE watermark = 0;

UPDATE ann_consumer_watermark
SET watermark = -2
WHERE watermark = 0
  AND EXISTS (
      SELECT 1
      FROM ann_consumer_pending pending
      WHERE pending.consumer = ann_consumer_watermark.consumer
        AND pending.namespace = ann_consumer_watermark.namespace
        AND pending.embedding_model = ann_consumer_watermark.embedding_model
  );
