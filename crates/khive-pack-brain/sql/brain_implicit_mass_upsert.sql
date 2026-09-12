INSERT INTO brain_implicit_mass (profile_id, namespace, target_id, mass, last_event_at, last_effective_weight)
VALUES (?1, ?2, ?3, ?4, ?5, ?6)
ON CONFLICT(profile_id, namespace, target_id)
DO UPDATE
SET mass = excluded.mass, last_event_at = excluded.last_event_at, last_effective_weight = excluded.last_effective_weight
