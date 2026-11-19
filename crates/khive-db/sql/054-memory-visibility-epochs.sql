-- An already-V46 database has no captured identities; creating the empty
-- inventory here must not capture its current notes retroactively.
CREATE TABLE IF NOT EXISTS memory_visibility_pre_v46 (
    note_id TEXT NOT NULL PRIMARY KEY REFERENCES notes(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS memory_visibility_epochs (
    note_id TEXT NOT NULL PRIMARY KEY REFERENCES notes(id) ON DELETE CASCADE,
    namespace TEXT NOT NULL,
    epoch TEXT NOT NULL CHECK(epoch IN ('legacy', 'modern', 'unknown'))
);

INSERT INTO memory_visibility_epochs (note_id, namespace, epoch)
SELECT n.id, n.namespace,
    CASE
        WHEN e.note_id IS NOT NULL AND (
            e.namespace IS NOT n.namespace OR e.epoch IS NULL
            OR e.epoch NOT IN ('legacy', 'modern', 'unknown')
        ) THEN 'unknown'
        WHEN e.epoch = 'unknown' THEN 'unknown'
        WHEN EXISTS (
            SELECT 1 FROM memory_visibility_receipts r
            WHERE r.note_id = n.id AND r.namespace <> n.namespace
        ) THEN 'unknown'
        WHEN EXISTS (
            SELECT 1 FROM memory_visibility_fences f
            WHERE f.note_id = n.id AND (
                f.namespace <> n.namespace OR NOT EXISTS (
                    SELECT 1 FROM memory_visibility_receipts r
                    WHERE r.note_id = f.note_id AND r.namespace = f.namespace
                )
            )
        ) THEN 'unknown'
        WHEN p.note_id IS NOT NULL AND e.epoch = 'modern' THEN 'unknown'
        WHEN (p.note_id IS NOT NULL OR e.epoch = 'legacy') AND EXISTS (
            SELECT 1 FROM memory_visibility_receipts r WHERE r.note_id = n.id
        ) THEN 'unknown'
        WHEN e.epoch = 'modern' THEN 'modern'
        WHEN p.note_id IS NOT NULL OR e.epoch = 'legacy' THEN 'legacy'
        WHEN EXISTS (
            SELECT 1 FROM memory_visibility_receipts r
            WHERE r.note_id = n.id AND r.namespace = n.namespace
              AND typeof(r.model_count) = 'integer' AND r.model_count >= 0
              AND r.model_count = (
                  SELECT COUNT(*) FROM memory_visibility_fences f
                  WHERE f.note_id = r.note_id AND f.namespace = r.namespace
              )
              AND r.model_count = (
                  SELECT COUNT(DISTINCT f.model) FROM memory_visibility_fences f
                  WHERE f.note_id = r.note_id AND f.namespace = r.namespace
              )
              AND NOT EXISTS (
                  SELECT 1 FROM memory_visibility_fences f
                  WHERE f.note_id = r.note_id AND f.namespace = r.namespace
                    AND (typeof(f.model) <> 'text' OR length(f.model) = 0
                         OR typeof(f.ann_write_log_seq) <> 'integer'
                         OR f.ann_write_log_seq <= 0)
              )
        ) THEN 'modern'
        ELSE 'unknown'
    END
FROM notes n
LEFT JOIN memory_visibility_pre_v46 p ON p.note_id = n.id
LEFT JOIN memory_visibility_epochs e ON e.note_id = n.id
WHERE n.kind = 'memory' AND n.key IS NOT NULL
ON CONFLICT(note_id) DO UPDATE SET
    namespace = excluded.namespace,
    epoch = excluded.epoch;
