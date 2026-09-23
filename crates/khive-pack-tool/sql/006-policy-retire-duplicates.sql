UPDATE tool_policy AS obsolete
SET deleted_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000000
WHERE obsolete.deleted_at IS NULL
    AND obsolete.id != (
        SELECT deciding.id FROM tool_policy AS deciding
        WHERE deciding.namespace = obsolete.namespace
            AND deciding.actor = obsolete.actor
            AND deciding.tool = obsolete.tool
            AND deciding.deleted_at IS NULL
        ORDER BY CASE deciding.decision WHEN 'deny' THEN 2 WHEN 'ask' THEN 1 ELSE 0 END DESC,
            deciding.created_at ASC, deciding.id ASC
        LIMIT 1
    );
