UPDATE charter_subjects
SET subject_epoch = subject_epoch + 1, candidate_digest = ?2, candidate_bytes = ?3, revision = revision + 1, updated_at_us = ?4
WHERE subject_id = ?1 AND revision = ?5
