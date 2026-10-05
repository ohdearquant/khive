-- Physical indexes for ordinary live entity pages and canonical type filters.
CREATE INDEX IF NOT EXISTS idx_entities_live_namespace_order
    ON entities(namespace, created_at DESC, id DESC) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_entities_live_namespace_type_order
    ON entities(namespace, entity_type, created_at DESC, id DESC) WHERE deleted_at IS NULL;
