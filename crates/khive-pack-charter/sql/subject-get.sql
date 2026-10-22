SELECT policy_domain, namespace, subject_id, subject_kind, subject_ref, subject_epoch, candidate_bytes, revision,
    (SELECT json_group_array(json_object('policy_domain', r.policy_domain, 'namespace', r.namespace))
     FROM charter_runs r WHERE r.subject_id = charter_subjects.subject_id) AS run_scopes
FROM charter_subjects
WHERE subject_kind = ?1 AND subject_ref = ?2
