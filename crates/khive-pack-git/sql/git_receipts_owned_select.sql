SELECT id, namespace, actor, session_id, verb, repo, inputs, gate, policy,
    fork_policy, credential, started_at, finished_at, disposition, result, reason
FROM git_receipts WHERE namespace = ?1 AND actor = ?2 AND id = ?3
