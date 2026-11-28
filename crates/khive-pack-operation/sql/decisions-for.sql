SELECT decision_id, subject_ids, type_version, form_revision, slot, reason, decided_by,
    decided_at_us, supersedes
FROM operation_slot_decisions
WHERE namespace = ?1 AND definition_id = ?2 AND form_id = ?3
ORDER BY decided_at_us, rowid
