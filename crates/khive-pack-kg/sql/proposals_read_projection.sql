SELECT proposal_id, proposer, status, approve_count, reject_count
FROM proposals_open
WHERE proposal_id = ?1
AND namespace = ?2
