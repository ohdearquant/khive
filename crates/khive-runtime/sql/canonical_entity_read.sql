SELECT id, namespace, kind, entity_type, name, description, properties, tags,
       created_at, updated_at, version, deleted_at, merged_into, merge_event_id,
       (SELECT content_ref FROM attachments
        WHERE record_uuid = entities.id AND substrate = 'entity' AND role = 'content') AS content_ref
FROM entities WHERE id = ?1
