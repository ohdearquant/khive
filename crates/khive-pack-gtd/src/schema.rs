//! GTD vocabulary: statuses, priorities, and lifecycle rules.
//!
//! Mirrors khive-internal's `services/note/src/schema.rs` GTD section so the OSS
//! pack stays interface-compatible with the internal task model.

/// Full lifecycle map for use in error messages and documentation.
pub const TASK_LIFECYCLE_HELP: &str = concat!(
    "inbox -> next | waiting | someday | active | done | cancelled; ",
    "next -> active | waiting | someday | done | cancelled; ",
    "active -> next | waiting | done | cancelled; ",
    "waiting -> next | active | done | cancelled; ",
    "someday -> next | active | done | cancelled; ",
    "done/cancelled -> terminal"
);

/// Canonical GTD statuses. Order is documentary, not lifecycle-derived.
pub const TASK_STATUSES: &[&str] = &[
    "inbox",
    "next",
    "waiting",
    "someday",
    "active",
    "done",
    "cancelled",
];

/// Canonical priorities. p0 highest, p3 lowest.
pub const VALID_PRIORITIES: &[&str] = &["p0", "p1", "p2", "p3"];

/// Map common aliases to canonical GTD status names.
pub fn normalize_status(s: &str) -> &str {
    match s {
        "in_progress" | "in-progress" | "started" | "working" => "active",
        "todo" | "backlog" => "inbox",
        "blocked" | "on_hold" | "on-hold" => "waiting",
        "later" | "maybe" => "someday",
        "finished" | "completed" | "closed" => "done",
        other => other,
    }
}

/// Return `true` when `s` (or its normalized alias) is a recognized GTD status.
pub fn is_valid_status(s: &str) -> bool {
    TASK_STATUSES.contains(&normalize_status(s))
}

/// Return `true` when `p` is a recognized priority level (`p0`..`p3`).
pub fn is_valid_priority(p: &str) -> bool {
    VALID_PRIORITIES.contains(&p.to_ascii_lowercase().as_str())
}

/// Map p0..p3 to a [0, 1] salience for hybrid search scoring.
pub fn priority_to_salience(p: &str) -> f64 {
    match p.to_ascii_lowercase().as_str() {
        "p0" => 1.0,
        "p1" => 0.75,
        "p2" => 0.5,
        "p3" => 0.25,
        _ => 0.5,
    }
}

/// True when a task in this status counts as "actionable now" — `next` returns these.
pub fn is_actionable(s: &str) -> bool {
    matches!(s, "next" | "active")
}

/// True when this is a terminal status that ends the task lifecycle.
pub fn is_terminal(s: &str) -> bool {
    matches!(s, "done" | "cancelled")
}

/// Allowed targets from a given status.
///
/// Lifecycle (mirrors khive-internal):
/// - `inbox`     → next | waiting | someday | active | done | cancelled
/// - `next`      → active | waiting | someday | done | cancelled
/// - `active`    → next | waiting | done | cancelled
/// - `waiting`   → next | active | done | cancelled
/// - `someday`   → next | active | done | cancelled
/// - `done`      → (terminal — no outgoing transitions)
/// - `cancelled` → (terminal — no outgoing transitions)
///
/// **Design decision (GTD-AUD-001 / issue #273)**: `done` and `cancelled` are
/// permanently terminal — they have no outgoing transitions. The implementation
/// explicitly closes terminal states to prevent accidental resurrection of
/// completed or abandoned work. This is the authoritative contract.
/// Use `gtd.assign` to create a new task if reopening semantics are required.
pub fn allowed_transitions(from: &str) -> &'static [&'static str] {
    match from {
        "inbox" => &["next", "waiting", "someday", "active", "done", "cancelled"],
        "next" => &["active", "waiting", "someday", "done", "cancelled"],
        "active" => &["next", "waiting", "done", "cancelled"],
        "waiting" => &["next", "active", "done", "cancelled"],
        "someday" => &["next", "active", "done", "cancelled"],
        "done" => &[],
        "cancelled" => &[],
        _ => &[],
    }
}

/// Return `true` when the GTD lifecycle permits a direct transition from `from` to `to`.
pub fn can_transition(from: &str, to: &str) -> bool {
    allowed_transitions(from).contains(&to)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_normalize_to_canonical() {
        assert_eq!(normalize_status("in_progress"), "active");
        assert_eq!(normalize_status("todo"), "inbox");
        assert_eq!(normalize_status("blocked"), "waiting");
        assert_eq!(normalize_status("later"), "someday");
        assert_eq!(normalize_status("finished"), "done");
        assert_eq!(normalize_status("done"), "done");
    }

    #[test]
    fn status_validation_accepts_aliases() {
        assert!(is_valid_status("inbox"));
        assert!(is_valid_status("in_progress"));
        assert!(is_valid_status("finished"));
        assert!(!is_valid_status("garbage"));
    }

    #[test]
    fn priority_validation_is_case_insensitive() {
        assert!(is_valid_priority("p0"));
        assert!(is_valid_priority("P1"));
        assert!(!is_valid_priority("p4"));
        assert!(!is_valid_priority("high"));
    }

    #[test]
    fn priority_to_salience_maps_tiers() {
        assert_eq!(priority_to_salience("p0"), 1.0);
        assert_eq!(priority_to_salience("p1"), 0.75);
        assert_eq!(priority_to_salience("p2"), 0.5);
        assert_eq!(priority_to_salience("p3"), 0.25);
        assert_eq!(priority_to_salience("unknown"), 0.5);
    }

    #[test]
    fn lifecycle_rules_match_documented_table() {
        assert!(can_transition("inbox", "next"));
        assert!(can_transition("next", "active"));
        assert!(can_transition("active", "done"));
        assert!(!can_transition("active", "inbox"));
        assert!(!can_transition("done", "waiting"));
        // Terminal states have no outgoing transitions (enforced per #273).
        assert!(!can_transition("done", "next"));
        assert!(!can_transition("done", "active"));
        assert!(!can_transition("cancelled", "next"));
        assert!(!can_transition("cancelled", "active"));
    }

    #[test]
    fn terminal_states_have_no_allowed_transitions() {
        assert!(
            allowed_transitions("done").is_empty(),
            "done must have no allowed transitions"
        );
        assert!(
            allowed_transitions("cancelled").is_empty(),
            "cancelled must have no allowed transitions"
        );
    }

    #[test]
    fn actionable_and_terminal_classification() {
        assert!(is_actionable("next"));
        assert!(is_actionable("active"));
        assert!(!is_actionable("inbox"));
        assert!(is_terminal("done"));
        assert!(is_terminal("cancelled"));
        assert!(!is_terminal("waiting"));
    }
}
