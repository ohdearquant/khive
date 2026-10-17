SELECT id FROM entities WHERE id = ?1
UNION ALL SELECT id FROM notes WHERE id = ?1
UNION ALL SELECT id FROM graph_edges WHERE id = ?1
UNION ALL SELECT id FROM events WHERE id = ?1
LIMIT 1
