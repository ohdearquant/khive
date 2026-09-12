UPDATE proposals_open
SET status = ?1, updated_at = ?2, last_decision = ?3,
    review_count = review_count + 1,
    approve_count = approve_count + ?4,
    reject_count = reject_count + ?5
WHERE proposal_id = ?6
AND namespace = ?7
AND status NOT IN ('applied', 'withdrawn', 'rejected', 'approved')
