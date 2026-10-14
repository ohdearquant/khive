UPDATE proposals_open
SET archived_at = ?1
WHERE namespace = ?2
AND status IN ('applied', 'rejected')
AND archived_at IS NULL
AND updated_at < ?3
