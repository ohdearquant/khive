SELECT id, namespace, kind, deleted_at, version, properties,
       created_at, updated_at,
       typeof(created_at) AS created_type,
       typeof(updated_at) AS updated_type,
       CASE
           WHEN properties IS NULL THEN 'object'
           WHEN json_valid(properties) THEN json_type(properties)
           ELSE 'invalid'
       END AS properties_type,
       CASE WHEN json_valid(properties) THEN properties -> '$.status' END AS stored_status_json,
       CASE WHEN json_valid(properties) THEN json_type(properties, '$.status') END AS stored_status_type,
       CASE WHEN json_valid(properties) THEN properties -> '$.gtd_repair' END AS repair_history_json
FROM notes
WHERE id = ?1
LIMIT 1
