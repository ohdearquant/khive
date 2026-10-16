SELECT index_type, length(snapshot), created_at
FROM retrieval_snapshots
WHERE namespace = ?1
