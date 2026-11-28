SELECT id, namespace, kind, entity_type, name, description, properties, tags,
       created_at, updated_at, version, deleted_at, merged_into, merge_event_id,
       (SELECT content_ref FROM attachments
        WHERE record_uuid = entities.id AND substrate = 'entity' AND role = 'content') AS content_ref,
       (SELECT json_group_array(json_array(entity_alias.alias, entity_alias.origin)) FROM entity_aliases AS entity_alias WHERE entity_alias.entity_id = entities.id AND entity_alias.namespace = entities.namespace) AS aliases
FROM entities WHERE id = ?1
