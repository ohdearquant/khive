CREATE INDEX IF NOT EXISTS idx_tool_grants_lookup ON tool_grants(namespace, actor, tool, status);
