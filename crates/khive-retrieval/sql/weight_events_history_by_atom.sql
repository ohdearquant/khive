SELECT ts, weight_after, delta, channel, context_id, event_id
FROM weight_events
WHERE namespace = ?1 AND atom_id = ?2
ORDER BY ts ASC
