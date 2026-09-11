CREATE INDEX IF NOT EXISTS idx_git_receipts_session
    ON git_receipts (namespace, actor, session_id);
