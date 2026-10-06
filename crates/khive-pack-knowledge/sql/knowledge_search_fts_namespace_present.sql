SELECT 1 AS present FROM fts_knowledge CROSS JOIN knowledge_atoms AS a ON a.rowid = fts_knowledge.rowid WHERE fts_knowledge MATCH ?1 AND +a.namespace = ?2 LIMIT 1
