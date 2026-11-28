CREATE TABLE IF NOT EXISTS operation_source_orders (
    namespace TEXT NOT NULL CHECK (length(namespace) > 0),
    version INTEGER NOT NULL CHECK (typeof(version) = 'integer' AND version > 0),
    digest TEXT NOT NULL CHECK (length(digest) > 0),
    order_bytes BLOB NOT NULL CHECK (typeof(order_bytes) = 'blob'),
    source_bytes TEXT NOT NULL CHECK (typeof(source_bytes) = 'text'),
    created_at_us INTEGER NOT NULL CHECK (created_at_us >= 0),
    PRIMARY KEY (namespace, version)
);

CREATE TRIGGER IF NOT EXISTS operation_source_orders_no_replace
BEFORE INSERT ON operation_source_orders
WHEN EXISTS (SELECT 1 FROM operation_source_orders
    WHERE namespace = NEW.namespace AND version = NEW.version)
BEGIN
    SELECT RAISE(ABORT, 'operation source orders are immutable');
END;

CREATE TRIGGER IF NOT EXISTS operation_source_orders_no_update
BEFORE UPDATE ON operation_source_orders
BEGIN
    SELECT RAISE(ABORT, 'operation source orders are immutable');
END;

CREATE TRIGGER IF NOT EXISTS operation_source_orders_no_delete
BEFORE DELETE ON operation_source_orders
BEGIN
    SELECT RAISE(ABORT, 'operation source orders are immutable');
END;
