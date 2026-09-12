SELECT snapshot_json, updated_at
FROM brain_profile_snapshots
WHERE profile_id = ?1
AND namespace = ?2
ORDER BY updated_at DESC
LIMIT 1
