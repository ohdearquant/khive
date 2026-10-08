SELECT atom_id, weight_after
FROM (
    SELECT atom_id, weight_after,
           ROW_NUMBER() OVER (PARTITION BY atom_id ORDER BY ts DESC) as rn
    FROM weight_events
    WHERE namespace = ?1 AND ts <= ?2
)
WHERE rn = 1
