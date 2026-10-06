CREATE INDEX IF NOT EXISTS idx_entities_live_namespace_kind_order
    ON entities(namespace, kind, created_at DESC, id DESC) WHERE deleted_at IS NULL;
