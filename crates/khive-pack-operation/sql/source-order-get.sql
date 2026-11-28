SELECT namespace, version, digest, order_bytes
FROM operation_source_orders
WHERE namespace = ?1 AND version = ?2
