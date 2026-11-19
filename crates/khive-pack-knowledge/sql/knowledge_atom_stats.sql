SELECT COUNT(*) AS total_atoms,
       COALESCE(SUM(CASE WHEN finalized = 1 THEN 1 ELSE 0 END), 0)
           AS finalized_atoms
FROM knowledge_atoms
WHERE namespace = ?1 AND deleted_at IS NULL
  AND NOT khive_tag_contains(tags, 'type:domain')
