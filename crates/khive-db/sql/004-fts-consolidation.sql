-- 004-fts-consolidation.sql
-- Consolidate per-namespace FTS5 tables into single shared tables.
-- Strategy: create unified tables (namespace UNINDEXED column).
-- Per-namespace partition cleanup is performed at runtime (namespace-agnostic sweep),
-- not in this SQL, to avoid hardcoding namespace names.
-- Repopulation: run `kkernel reindex --no-knowledge` after this migration.
-- Out of scope: fts_knowledge, fts_sections (knowledge-pack corpus).

-- Unified entity FTS table (one table for all namespaces; namespace is a filter column)
CREATE VIRTUAL TABLE IF NOT EXISTS fts_entities USING fts5(
    subject_id UNINDEXED,
    kind UNINDEXED,
    title,
    body,
    tags UNINDEXED,
    namespace UNINDEXED,
    metadata UNINDEXED,
    updated_at UNINDEXED,
    tokenize = 'trigram'
);

-- Unified notes FTS table (one table for all namespaces; namespace is a filter column)
CREATE VIRTUAL TABLE IF NOT EXISTS fts_notes USING fts5(
    subject_id UNINDEXED,
    kind UNINDEXED,
    title,
    body,
    tags UNINDEXED,
    namespace UNINDEXED,
    metadata UNINDEXED,
    updated_at UNINDEXED,
    tokenize = 'trigram'
);
