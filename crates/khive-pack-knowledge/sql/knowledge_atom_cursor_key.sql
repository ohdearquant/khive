SELECT created_at, id FROM knowledge_atoms
WHERE namespace = ?1 AND id = ?2 AND NOT khive_tag_contains(tags, 'type:domain') LIMIT 1
