SELECT id, name FROM entities
WHERE kind = 'project' AND deleted_at IS NULL
ORDER BY name;
