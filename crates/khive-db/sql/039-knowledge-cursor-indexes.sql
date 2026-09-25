CREATE INDEX IF NOT EXISTS idx_knowledge_atoms_cursor
    ON knowledge_atoms(namespace, created_at, id)
    WHERE deleted_at IS NULL AND tags NOT LIKE '%type:domain%';
CREATE INDEX IF NOT EXISTS idx_knowledge_domains_cursor
    ON knowledge_domains(namespace, created_at, id)
    WHERE deleted_at IS NULL;
