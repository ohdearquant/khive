SELECT CASE WHEN length(CAST(merged_into AS BLOB))=36 THEN merged_into ELSE NULL END
FROM entities WHERE id=?1 AND namespace=?2
AND kind='project' AND deleted_at IS NOT NULL
