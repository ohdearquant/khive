UPDATE tool_policy
SET deleted_at = ?1, deleted_by = ?2
WHERE namespace = ?3
    AND deleted_at IS NULL
    AND CAST(tool AS BLOB) = CAST(?4 AS BLOB)
    AND instr(CAST(tool AS BLOB), X'2A') = 0;
