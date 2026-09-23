UPDATE tool_policy AS survivor
SET history = (
    SELECT json_group_array(json(value))
    FROM (
        SELECT value FROM json_each(survivor.history)
        UNION ALL
        SELECT json_object(
            'id', legacy.id,
            'decision', legacy.decision,
            'note', legacy.note,
            'timestamp', legacy.updated_at,
            'author', legacy.updated_by,
            'prior_history', json(legacy.history),
            'reason', 'legacy_consolidation',
            'consolidated_at', CAST(strftime('%s', 'now') AS INTEGER) * 1000000
        )
        FROM (
            SELECT * FROM tool_policy AS candidate
            WHERE candidate.namespace = survivor.namespace
                AND candidate.actor = survivor.actor
                AND candidate.tool = survivor.tool
                AND candidate.deleted_at IS NULL
                AND candidate.id != survivor.id
            ORDER BY candidate.created_at ASC, candidate.id ASC
        ) AS legacy
    )
)
WHERE survivor.deleted_at IS NULL
    AND survivor.id = (
        SELECT deciding.id FROM tool_policy AS deciding
        WHERE deciding.namespace = survivor.namespace
            AND deciding.actor = survivor.actor
            AND deciding.tool = survivor.tool
            AND deciding.deleted_at IS NULL
        ORDER BY CASE deciding.decision WHEN 'deny' THEN 2 WHEN 'ask' THEN 1 ELSE 0 END DESC,
            deciding.created_at ASC, deciding.id ASC
        LIMIT 1
    )
    AND EXISTS (
        SELECT 1 FROM tool_policy AS duplicate
        WHERE duplicate.namespace = survivor.namespace
            AND duplicate.actor = survivor.actor
            AND duplicate.tool = survivor.tool
            AND duplicate.deleted_at IS NULL
            AND duplicate.id != survivor.id
    );
