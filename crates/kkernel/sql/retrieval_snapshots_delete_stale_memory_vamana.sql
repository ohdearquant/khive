DELETE FROM retrieval_snapshots
WHERE index_type = 'memory_vamana'
  AND namespace NOT GLOB ?1
