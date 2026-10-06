-- Core-table indexes are migrated independently of comm pack loading.
CREATE INDEX IF NOT EXISTS idx_comm_message_direction ON notes(namespace, kind, json_extract(properties, '$.direction'), json_extract(properties, '$.read'), created_at DESC) WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_comm_message_thread ON notes(namespace, kind, json_extract(properties, '$.thread_id'), created_at DESC) WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_comm_message_to_actor ON notes(namespace, kind, json_extract(properties, '$.to_actor'), json_extract(properties, '$.direction'), json_extract(properties, '$.read'), created_at DESC) WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_comm_message_outbound_ref ON notes(namespace, kind, json_extract(properties, '$.direction'), json_extract(properties, '$.from_actor'), json_extract(properties, '$.outbound_ref')) WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_comm_message_outbound_recipient ON notes(namespace, kind, json_extract(properties, '$.direction'), json_extract(properties, '$.to_actor'), created_at DESC, id ASC) WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_comm_quarantine_expiry ON notes(namespace, kind, json_extract(properties, '$.channel_kind'), json_extract(properties, '$.channel_slug'), expires_at, id) WHERE deleted_at IS NULL AND expires_at IS NOT NULL;
