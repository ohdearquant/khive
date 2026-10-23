SELECT phase_id, ordinal, state, assignee_kind, assignee, round, entered_sequence, waiting_on, updated_at_us FROM charter_phases
WHERE policy_domain = ?1 AND run_id = ?2 ORDER BY ordinal
