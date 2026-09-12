UPDATE proposals_open
SET updated_at = ?1, last_decision = ?2,
    review_count = review_count + 1
WHERE proposal_id = ?3
AND namespace = ?4
