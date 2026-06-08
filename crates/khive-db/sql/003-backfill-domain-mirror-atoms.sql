-- V3: Backfill mirror atoms for existing knowledge_domains.
--
-- upsert_domains dual-writes to knowledge_atoms, but domains imported before
-- the dual-write was added have no mirror atom. This migration creates one for
-- each domain, giving them FTS + embedding coverage via the normal atom pipeline.
--
-- Safety: INSERT OR IGNORE skips domains whose slug collides with an existing
-- non-domain atom. A collision means a real atom owns that slug — the domain
-- mirror must not overwrite it.

INSERT OR IGNORE INTO knowledge_atoms (
    id, namespace, slug, name, content, tags, properties,
    status, finalized, created_at, updated_at
)
SELECT
    d.id,
    d.namespace,
    d.slug,
    d.name,
    COALESCE(d.description, ''),
    CASE
        WHEN d.tags LIKE '%type:domain%' THEN d.tags
        WHEN d.tags IS NULL OR d.tags = '[]' THEN '["type:domain"]'
        ELSE substr(d.tags, 1, length(d.tags) - 1) || ',"type:domain"]'
    END,
    '{"members":' || COALESCE(d.members, '[]') || '}',
    'reviewed',
    1,
    d.created_at,
    d.updated_at
FROM knowledge_domains d
WHERE d.deleted_at IS NULL;
