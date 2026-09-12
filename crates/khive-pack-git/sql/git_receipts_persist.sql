UPDATE git_receipts SET gate = ?1, policy = ?2, fork_policy = ?3,
credential = ?4, finished_at = ?5, disposition = ?6, result = ?7, reason = ?8
WHERE id = ?9 AND namespace = ?10 AND actor = ?11 AND disposition = 'unknown'
AND session_id IS ?12 AND verb = ?13 AND repo = ?14 AND inputs = ?15
AND started_at = ?16
