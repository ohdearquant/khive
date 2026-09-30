-- On-demand audit for event profile versions that the event reader cannot decode.
SELECT id,
       namespace,
       typeof(profile_state_version) AS stored_type,
       quote(profile_state_version) AS stored_value
FROM events
WHERE profile_state_version IS NOT NULL
  AND (typeof(profile_state_version) <> 'integer'
       OR profile_state_version < 0)
ORDER BY id;
