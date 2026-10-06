SELECT * FROM knowledge_sections
WHERE atom_id = ?1 AND namespace = ?2
ORDER BY sort_order ASC, created_at ASC, id ASC
