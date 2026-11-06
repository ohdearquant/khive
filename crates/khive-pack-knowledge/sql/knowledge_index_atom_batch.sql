SELECT atom.*
FROM json_each(?2) AS selected
JOIN knowledge_atoms AS atom ON atom.id = selected.value
WHERE atom.namespace = ?1 AND atom.deleted_at IS NULL
ORDER BY CAST(selected.key AS INTEGER)
