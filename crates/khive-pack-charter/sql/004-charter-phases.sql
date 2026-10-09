CREATE TABLE IF NOT EXISTS charter_phases (
    policy_domain TEXT NOT NULL,
    run_id TEXT NOT NULL,
    phase_id TEXT NOT NULL CHECK (length(phase_id) > 0),
    namespace TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (typeof(ordinal) = 'integer' AND ordinal > 0),
    state TEXT NOT NULL CHECK (state IN ('dormant', 'waiting_trigger', 'waiting_gate', 'waiting_actor', 'ready', 'executing', 'uncertain', 'completed')),
    assignee_kind TEXT NOT NULL CHECK (assignee_kind IN ('actor', 'role')),
    assignee TEXT NOT NULL CHECK (length(assignee) > 0),
    revision INTEGER NOT NULL DEFAULT 0 CHECK (typeof(revision) = 'integer' AND revision >= 0),
    created_at_us INTEGER NOT NULL CHECK (created_at_us >= 0),
    updated_at_us INTEGER NOT NULL CHECK (updated_at_us >= created_at_us),
    PRIMARY KEY (policy_domain, run_id, phase_id),
    UNIQUE (policy_domain, run_id, ordinal),
    FOREIGN KEY (policy_domain, run_id)
        REFERENCES charter_runs (policy_domain, run_id)
);
