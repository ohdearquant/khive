SELECT 1 AS present FROM knowledge_atoms AS a INDEXED BY idx_knowledge_atoms_ns CROSS JOIN fts_knowledge ON fts_knowledge.rowid = a.rowid WHERE a.namespace = ?2 AND fts_knowledge MATCH ?1 LIMIT 1
