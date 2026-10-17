INSERT INTO event_observations
       (event_id, entity_id, referent_kind, role, position)
SELECT ?1, ?2, ?3, ?4, ?5
WHERE (changes() = 1)
