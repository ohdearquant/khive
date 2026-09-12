UPDATE proposals_open
SET status = 'applied', updated_at = ?1
WHERE proposal_id = ?2
AND namespace = ?3
AND status = 'applying'
