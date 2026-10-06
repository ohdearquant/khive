UPDATE knowledge_sections SET embedding=NULL, updated_at=?1
WHERE atom_id=?2 AND namespace=?3 AND embedding IS NOT NULL
AND EXISTS (SELECT 1 FROM knowledge_atoms a
            WHERE a.id=?2 AND a.namespace=?3 AND a.name<>?4)
