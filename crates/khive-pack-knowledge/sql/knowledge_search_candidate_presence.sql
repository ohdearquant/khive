SELECT id FROM knowledge_atoms WHERE id IN (/* ids */ ?1) UNION SELECT id FROM knowledge_domains WHERE id IN (/* ids */ ?1)
