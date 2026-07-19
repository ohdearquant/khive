-- ANN write log: per-vector-write delta records consumed by the restart
-- classifier (ADR-079 Amendment 1). AUTOINCREMENT is load-bearing: seq must be
-- strictly monotone and never reused so a persisted watermark stays comparable
-- across log compactions.
CREATE TABLE IF NOT EXISTS ann_write_log (
    seq             INTEGER PRIMARY KEY AUTOINCREMENT,
    namespace       TEXT NOT NULL,
    embedding_model TEXT NOT NULL,
    subject_id      TEXT NOT NULL,
    op              TEXT NOT NULL CHECK (op IN ('upsert', 'delete'))
);

CREATE INDEX IF NOT EXISTS idx_ann_write_log_ns_model_seq
    ON ann_write_log (namespace, embedding_model, seq);
