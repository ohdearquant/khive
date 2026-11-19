SELECT index_type, length(snapshot) AS snapshot_size, created_at
FROM retrieval_snapshots
WHERE namespace = ?1
