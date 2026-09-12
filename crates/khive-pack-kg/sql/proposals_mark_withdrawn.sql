UPDATE proposals_open
SET status = 'withdrawn', updated_at = ?1
WHERE proposal_id = ?2
AND namespace = ?3
AND status NOT IN ('applied', 'applying', 'withdrawn', 'rejected')
