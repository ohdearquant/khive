-- ADR-046 C2: retain proposal identity and history; only ordinary lists hide archives.
ALTER TABLE proposals_open ADD COLUMN archived_at INTEGER;
