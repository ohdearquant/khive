SELECT proposal_id
FROM proposals_open
WHERE proposal_id = ?1
AND namespace = ?2
LIMIT 1
