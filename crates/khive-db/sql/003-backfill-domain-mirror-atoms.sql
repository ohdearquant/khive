-- V3: Backfill mirror atoms for existing knowledge_domains.
--
-- upsert_domains dual-writes to knowledge_atoms, but domains imported before
-- the dual-write was added have no mirror atom. This migration creates one for
-- each domain, giving them FTS + embedding coverage via the normal atom pipeline.
-- ON CONFLICT handles the idempotent case.
--
-- Content = description. Tags include "type:domain". atom_embed_text enriches
-- the embed input with name + content + tags at embed time.

INSERT INTO knowledge_atoms (
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
WHERE d.deleted_at IS NULL
ON CONFLICT(namespace, slug) DO UPDATE SET
    name       = excluded.name,
    content    = excluded.content,
    tags       = excluded.tags,
    properties = excluded.properties,
    status     = 'reviewed',
    finalized  = 1,
    updated_at = excluded.updated_at;
