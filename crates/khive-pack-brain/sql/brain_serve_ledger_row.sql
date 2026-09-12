SELECT id, namespace, consumer_kind, served_by_profile_id, resolved_profile_id, resolved_at, accounting_profile_id, target_id, query_class, query_raw, served_at, grade, graded_at, scorer_run_id, serve_attribution
FROM brain_serve_ledger
WHERE id = ?1
