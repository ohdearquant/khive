-- Classify tags at read time using the shared decoded/legacy rule. The index
-- must include near-marker atoms and must not depend on an application UDF.
DROP INDEX IF EXISTS idx_knowledge_atoms_cursor;
CREATE INDEX idx_knowledge_atoms_cursor
ON knowledge_atoms(namespace, created_at, id)
WHERE deleted_at IS NULL;
