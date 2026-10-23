CREATE TABLE IF NOT EXISTS charter_waiting_changes (
    commit_order INTEGER PRIMARY KEY AUTOINCREMENT,
    policy_domain TEXT NOT NULL,
    namespace TEXT NOT NULL,
    run_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 0),
    view_bytes TEXT NOT NULL CHECK (typeof(view_bytes) = 'text'),
    FOREIGN KEY (policy_domain, run_id)
        REFERENCES charter_runs (policy_domain, run_id)
);

CREATE INDEX IF NOT EXISTS charter_waiting_changes_scope
    ON charter_waiting_changes (policy_domain, namespace, commit_order);

CREATE VIEW IF NOT EXISTS charter_waiting_snapshot AS
SELECT
    r.policy_domain,
    r.namespace,
    r.run_id,
    r.revision,
    json_object(
        'run_id', r.run_id,
        'charter', r.charter_id,
        'version', r.definition_version,
        'assurance', r.assurance,
        'parent_run_id', r.parent_run_id,
        'subject', json_object('kind', s.subject_kind, 'ref', s.subject_ref),
        'phase', p.phase_id,
        'state', COALESCE(r.terminal_outcome, r.state),
        'round', coalesce(p.round, 0),
        'assignee_kind', p.assignee_kind,
        'assignee', p.assignee,
        'owed_by', json('[]'),
        'candidate', json(CAST(r.candidate_bytes AS TEXT)),
        'waiting_on', CASE WHEN p.waiting_on IS NULL THEN json('null') ELSE json(p.waiting_on) END,
        'since_us', coalesce(p.updated_at_us, r.updated_at_us),
        'revision', r.revision,
        'evaluation', json_object(
            'phase', p.phase_id,
            'status', CASE WHEN r.state <> 'open' THEN COALESCE(r.terminal_outcome, r.state) ELSE coalesce(p.state, 'waiting') END,
            'failing_gate_ids', json(coalesce((
                SELECT json_group_array(json_extract(blocked.value, '$.require'))
                FROM json_each(coalesce(json_extract(p.waiting_on, '$.blocked'), '[]')) AS blocked
                WHERE json_extract(blocked.value, '$.require') IS NOT NULL
            ), '[]')),
            'reason', CASE WHEN p.waiting_on IS NULL THEN NULL ELSE json(p.waiting_on) END
        )
    ) AS view_bytes
FROM charter_runs AS r
JOIN charter_subjects AS s ON s.subject_id = r.subject_id
LEFT JOIN charter_phases AS p
    ON p.policy_domain = r.policy_domain
    AND p.run_id = r.run_id
    AND p.phase_id = r.current_phase_id;

CREATE TRIGGER IF NOT EXISTS charter_waiting_changes_run_update
AFTER UPDATE ON charter_runs
WHEN NEW.revision <> OLD.revision OR NEW.state <> OLD.state
    OR NEW.terminal_outcome IS NOT OLD.terminal_outcome
    OR NEW.current_phase_id IS NOT OLD.current_phase_id
BEGIN
    INSERT INTO charter_waiting_changes (policy_domain, namespace, run_id, revision, view_bytes)
    SELECT policy_domain, namespace, run_id, revision, view_bytes
    FROM charter_waiting_snapshot
    WHERE policy_domain = NEW.policy_domain AND run_id = NEW.run_id;
END;

CREATE TRIGGER IF NOT EXISTS charter_waiting_changes_phase_update
AFTER UPDATE ON charter_phases
WHEN NEW.state <> OLD.state OR NEW.round <> OLD.round
    OR NEW.waiting_on IS NOT OLD.waiting_on OR NEW.revision <> OLD.revision
BEGIN
    INSERT INTO charter_waiting_changes (policy_domain, namespace, run_id, revision, view_bytes)
    SELECT policy_domain, namespace, run_id, revision, view_bytes
    FROM charter_waiting_snapshot
    WHERE policy_domain = NEW.policy_domain AND run_id = NEW.run_id;
END;

INSERT INTO charter_waiting_changes (policy_domain, namespace, run_id, revision, view_bytes)
SELECT policy_domain, namespace, run_id, revision, view_bytes
FROM charter_waiting_snapshot
WHERE json_extract(view_bytes, '$.phase') IS NOT NULL;
