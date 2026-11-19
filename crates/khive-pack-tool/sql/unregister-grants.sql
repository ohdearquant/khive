UPDATE tool_grants
SET status = 'revoked', decided_at = ?1, decided_by = ?2,
    decision_note = 'Registry registration retired'
WHERE namespace = ?3
    AND status <> 'revoked'
    AND (
        registry_id = ?4
        OR (
            registry_id IS NULL AND definition_digest IS NULL
            AND status IN ('requested', 'granted', 'denied')
            AND CAST(tool AS BLOB) = CAST(?5 AS BLOB)
            AND instr(CAST(tool AS BLOB), X'2A') = 0
        )
    );
