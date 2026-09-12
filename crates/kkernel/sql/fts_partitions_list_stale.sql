SELECT name FROM sqlite_master
WHERE type IN ('table', 'shadow')
  AND (name LIKE 'fts_entities_%' OR name LIKE 'fts_notes_%')
