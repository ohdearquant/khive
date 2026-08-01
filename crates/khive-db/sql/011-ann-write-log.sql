-- ANN write log: per-vector-write delta records consumed by the restart
-- classifier (ADR-079 Amendment 1). AUTOINCREMENT is load-bearing: seq must be
-- strictly monotone and never reused so a persisted watermark stays comparable
-- across log compactions. kind/field carry the vector row's own scope so a
-- consumer whose corpus is a subset of a shared vec table can filter its tail
-- with the same predicate as its corpus scan.
CREATE TABLE IF NOT EXISTS ann_write_log (
    seq             INTEGER PRIMARY KEY AUTOINCREMENT,
    namespace       TEXT NOT NULL,
    embedding_model TEXT NOT NULL,
    kind            TEXT NOT NULL,
    field           TEXT NOT NULL,
    subject_id      TEXT NOT NULL,
    op              TEXT NOT NULL CHECK (op IN ('upsert', 'delete'))
);

CREATE INDEX IF NOT EXISTS idx_ann_write_log_ns_model_seq
    ON ann_write_log (namespace, embedding_model, seq);

-- Durable per-consumer watermark registry gating log compaction. A consumer
-- normally registers its row at watermark 0 (INSERT OR IGNORE) before persist
-- or serve, then raises it monotonically after each segment commit. The
-- knowledge consumer additionally uses -1 as a closed force-rebuild sentinel:
-- every absent row writes -1 before an authoritative scan (local first-use
-- evidence cannot rule out a stale peer), and only that consumer's fenced full
-- checkpoint may transition it to S >= 0. Compaction
-- deletes only seq <= MIN(watermark) over the pair's registered rows, so 0 and
-- -1 both block unsafe deletion; stale/crash-frozen rows only under-compact.
CREATE TABLE IF NOT EXISTS ann_consumer_watermark (
    consumer        TEXT NOT NULL,
    namespace       TEXT NOT NULL,
    embedding_model TEXT NOT NULL,
    watermark       INTEGER NOT NULL,
    PRIMARY KEY (consumer, namespace, embedding_model)
);
