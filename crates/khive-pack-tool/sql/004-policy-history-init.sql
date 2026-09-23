UPDATE tool_policy
SET updated_at = coalesce(updated_at, created_at),
    updated_by = coalesce(updated_by, created_by),
    history = coalesce(history, '[]')
WHERE updated_at IS NULL OR history IS NULL;
