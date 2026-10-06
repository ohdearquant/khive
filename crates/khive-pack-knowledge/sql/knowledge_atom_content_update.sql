UPDATE knowledge_atoms SET name=?1, content=?2, tags=?3, properties=?4,
  source_uri=CASE WHEN ?5 = 1 THEN ?6 ELSE source_uri END,
  source_type=CASE WHEN ?7 = 1 THEN ?8 ELSE source_type END,
  finalized=CASE WHEN ?9 = 1 THEN ?10 ELSE finalized END,
  status=CASE WHEN ?9 = 1 AND ?10 = 1 AND status = 'draft' THEN 'reviewed' ELSE status END,
  updated_at=?11
WHERE id=?12 AND namespace=?13
