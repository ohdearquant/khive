SELECT subject_id, subject_kind, subject_ref, subject_epoch, candidate_bytes, revision FROM charter_subjects
WHERE subject_kind = ?1 AND subject_ref = ?2
