SELECT proposal_id, proposer, title, status, created_at, updated_at,
       expiry, last_decision, review_count, approve_count, reject_count
FROM proposals_open
WHERE namespace = ?1
AND (?2 IS NULL OR status = ?2)
AND (?3 IS NULL OR proposer = ?3)
ORDER BY updated_at DESC, proposal_id DESC
LIMIT ?4 OFFSET ?5
