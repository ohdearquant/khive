INSERT INTO proposals_open
       (proposal_id, namespace, proposer, title, status,
        created_at, updated_at, expiry)
VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?5, ?6)
