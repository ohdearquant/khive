SELECT mass, last_event_at
FROM brain_implicit_mass
WHERE profile_id = ?1
AND namespace = ?2
AND target_id = ?3
