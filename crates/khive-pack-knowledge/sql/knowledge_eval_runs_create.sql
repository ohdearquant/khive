CREATE TABLE IF NOT EXISTS knowledge_eval_runs (
    id              TEXT PRIMARY KEY,
    namespace       TEXT NOT NULL,
    run_at          INTEGER NOT NULL,
    query_set       TEXT NOT NULL,
    total_queries   INTEGER NOT NULL,
    precision_at_5  REAL NOT NULL,
    recall_at_5     REAL NOT NULL,
    mrr             REAL NOT NULL,
    notes           TEXT
)
