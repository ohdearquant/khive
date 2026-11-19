SELECT subject_id, op, MAX(seq) AS seq FROM ann_write_log WHERE namespace = ?1 AND embedding_model = ?2 AND field = 'knowledge.atom' AND seq > ?3 GROUP BY subject_id
