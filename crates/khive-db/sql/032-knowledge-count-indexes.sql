-- LIKE uses SQLite's default ASCII case folding; match it without changing
-- the stored verb or the count's predicate.
CREATE INDEX IF NOT EXISTS idx_events_ns_verb ON events(namespace, verb COLLATE NOCASE);

-- Cover the live atom count, optional lifecycle filter, and finalized sum
-- without fetching each atom's content-bearing table page.
CREATE INDEX IF NOT EXISTS idx_knowledge_atoms_ns_live_counts
    ON knowledge_atoms(namespace, deleted_at, status, tags, finalized);
