SELECT r.run_id, r.charter_id, r.definition_version, r.state, r.current_phase_id, r.parent_run_id, r.created_at_us, r.updated_at_us, s.subject_kind, s.subject_ref
FROM charter_runs r JOIN charter_subjects s ON s.subject_id = r.subject_id
WHERE r.policy_domain = ?1 AND (?2 IS NULL OR r.charter_id = ?2) AND (?3 IS NULL OR s.subject_kind = ?3)
    AND (?4 IS NULL OR s.subject_ref = ?4) AND (?5 IS NULL OR r.state = ?5)
ORDER BY r.updated_at_us DESC, r.run_id LIMIT ?6
