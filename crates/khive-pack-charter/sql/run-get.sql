SELECT r.policy_domain, r.run_id, r.charter_id, r.definition_version, r.definition_digest, r.subject_id, r.subject_epoch, r.candidate_bytes, r.assurance, r.namespace, COALESCE(r.terminal_outcome, r.state) AS state, r.current_phase_id, r.revision, r.parent_run_id, r.created_at_us, r.updated_at_us, s.subject_kind, s.subject_ref
FROM charter_runs r JOIN charter_subjects s ON s.subject_id = r.subject_id
WHERE r.run_id = ?1
