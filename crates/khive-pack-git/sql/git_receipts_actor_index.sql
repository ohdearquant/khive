CREATE INDEX IF NOT EXISTS idx_git_receipts_actor
    ON git_receipts (namespace, actor);
