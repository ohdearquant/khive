SELECT updated_at
FROM brain_profile_snapshots
WHERE profile_id = ?1
AND namespace = ?2
