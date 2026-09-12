SELECT id, namespace, actor, session_id, verb, repo, inputs, gate, policy,
    fork_policy, credential, started_at, finished_at, disposition, result, reason
FROM git_receipts WHERE namespace = ?1 AND actor = ?2
AND (?3 IS NULL OR repo = ?3) AND (?4 IS NULL OR session_id = ?4)
ORDER BY rowid ASC LIMIT ?5 OFFSET ?6
