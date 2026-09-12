INSERT INTO brain_profile_snapshots (profile_id, namespace, snapshot_json, updated_at)
VALUES (?1, ?2, ?3, ?4)
ON CONFLICT(profile_id, namespace)
DO UPDATE
SET snapshot_json = excluded.snapshot_json, updated_at = excluded.updated_at
