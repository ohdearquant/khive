CREATE UNIQUE INDEX IF NOT EXISTS idx_tool_policy_live
    ON tool_policy(namespace, actor, tool)
    WHERE deleted_at IS NULL;
