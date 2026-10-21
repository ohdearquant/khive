SELECT r.run_id, r.charter_id, r.definition_version, r.candidate_bytes, r.parent_run_id, s.subject_kind, s.subject_ref,
    p.phase_id, p.state, p.round, p.assignee_kind, p.assignee, p.waiting_on, p.updated_at_us
FROM charter_runs r
JOIN charter_subjects s ON s.subject_id = r.subject_id
JOIN charter_phases p ON p.policy_domain = r.policy_domain AND p.run_id = r.run_id AND p.phase_id = r.current_phase_id
WHERE r.policy_domain = ?1 AND r.state = 'open' AND (?2 IS NULL OR r.charter_id = ?2) AND (?3 IS NULL OR s.subject_kind = ?3)
ORDER BY p.updated_at_us, r.run_id
