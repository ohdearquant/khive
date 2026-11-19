CREATE INDEX IF NOT EXISTS idx_knowledge_eval_runs_ns_run_at ON knowledge_eval_runs(namespace, run_at DESC)
