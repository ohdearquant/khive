INSERT INTO brain_serve_ledger (id, namespace, consumer_kind, served_by_profile_id, resolved_profile_id, resolved_at, target_id, query_class, query_raw, served_at, serve_attribution)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
ON CONFLICT(namespace, target_id, query_class, served_at)
DO NOTHING
