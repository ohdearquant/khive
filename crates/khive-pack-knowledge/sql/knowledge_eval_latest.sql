SELECT run_at, precision_at_5, mrr FROM knowledge_eval_runs
WHERE namespace = ?1 ORDER BY run_at DESC, rowid DESC LIMIT 1
