SELECT a.id AS record_id, 'atom' AS record_type,
       json_object(
           'type', 'atom', 'id', a.id, 'namespace', a.namespace,
           'slug', a.slug, 'name', a.name, 'content', a.content,
           'tags', json(a.tags), 'properties', json(a.properties),
           'finalized', json(CASE WHEN a.finalized = 1 THEN 'true' ELSE 'false' END),
           'status', a.status, 'source_uri', a.source_uri, 'source_type', a.source_type,
           'created_at', a.created_at, 'updated_at', a.updated_at,
           'deleted_at', a.deleted_at
       ) AS record
FROM knowledge_atoms AS a
WHERE a.namespace = ?1 AND a.deleted_at IS NULL
UNION ALL
SELECT d.id, 'domain',
       json_object(
           'type', 'domain', 'id', d.id, 'namespace', d.namespace,
           'slug', d.slug, 'name', d.name, 'description', d.description,
           'tags', json(d.tags), 'members', json(d.members), 'status', d.status,
           'created_at', d.created_at, 'updated_at', d.updated_at,
           'deleted_at', d.deleted_at
       )
FROM knowledge_domains AS d
WHERE d.namespace = ?1 AND d.deleted_at IS NULL
UNION ALL
SELECT s.id, 'section',
       json_object(
           'type', 'section', 'id', s.id, 'atom_id', s.atom_id,
           'namespace', s.namespace, 'section_type', s.section_type,
           'heading', s.heading, 'content', s.content, 'content_hash', s.content_hash,
           'tokens', s.tokens, 'sort_order', s.sort_order, 'status', s.status,
           'created_at', s.created_at, 'updated_at', s.updated_at
       )
FROM knowledge_sections AS s
JOIN knowledge_atoms AS a ON a.id = s.atom_id AND a.namespace = s.namespace
WHERE s.namespace = ?1 AND a.deleted_at IS NULL
ORDER BY record_id COLLATE BINARY, record_type COLLATE BINARY
