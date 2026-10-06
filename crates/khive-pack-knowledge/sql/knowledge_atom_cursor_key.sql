SELECT created_at, id FROM knowledge_atoms WHERE namespace = ?1 AND id = ?2 AND tags NOT LIKE '%type:domain%' LIMIT 1
