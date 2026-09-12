SELECT proposal_id
FROM proposals_open
WHERE proposal_id LIKE ?1
AND namespace = ?2
LIMIT 2
