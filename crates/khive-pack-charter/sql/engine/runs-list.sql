SELECT r.run_id, r.charter_id, r.definition_version, r.assurance, COALESCE(r.terminal_outcome, r.state) AS state, r.current_phase_id, r.parent_run_id, r.created_at_us, r.updated_at_us, s.subject_kind, s.subject_ref
FROM charter_runs r JOIN charter_subjects s ON s.subject_id = r.subject_id
WHERE r.policy_domain = ?1 AND r.namespace IN (SELECT value FROM json_each(?2))
    AND (?3 IS NULL OR r.charter_id = ?3) AND (?4 IS NULL OR s.subject_kind = ?4)
    AND (?5 IS NULL OR s.subject_ref = ?5) AND (?6 IS NULL OR COALESCE(r.terminal_outcome, r.state) = ?6)
ORDER BY r.updated_at_us DESC, r.run_id LIMIT ?7
