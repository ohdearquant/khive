SELECT target_id AS id FROM graph_edges
WHERE namespace = ?1 AND source_id = ?2 AND relation = 'annotates'
