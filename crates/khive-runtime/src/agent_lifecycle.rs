//! Pure lifecycle logic for the runtime-owned agent process record (ADR-142 §1).
//!
//! No database, no I/O: state transitions and the spawn fingerprint are derived
//! entirely from in-memory values. `AgentState`, `TerminalReason`, and
//! `AgentRecord` live in `khive_types` (re-exported here) so `khive-db` and
//! `khive-pack-agent` can share them without depending on `khive-runtime`.

use khive_types::hash::Hash32;
pub use khive_types::{AgentRecord, AgentState, TerminalReason};

/// The event driving a lifecycle transition attempt. Distinct from the wire-level verbs
/// (`agent.spawn`/`agent.resume`/`agent.kill`/`agent.suspend`/`agent.observe`): `Dispatch`,
/// `Activity`, `Complete`, `Fail`, `Abandon`, and `HostRestart` are runtime-internal triggers
/// that ADR-142's table drives automatically rather than through a caller-issued verb.
/// `agent.spawn` itself is not a `Trigger` — it creates a record rather than transitioning
/// an existing one, so it is out of scope for this function (see the module-level tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The provider begins producing the first turn (`spawned` -> `running`).
    Dispatch,
    /// A subsequent round of an already-running turn (`running` -> `running`, a no-op:
    /// no state change is persisted and `state_changed_at` is not refreshed).
    Activity,
    Suspend,
    Resume,
    Kill,
    /// The provider returns its terminal result with no further tool calls pending.
    Complete,
    /// An unrecoverable provider or dispatch error.
    Fail,
    /// The record's persistent native attachment disconnects without an explicit kill.
    Abandon,
    /// Runtime restart boot scan.
    HostRestart,
}

/// The outcome of a successful transition attempt, including no-ops.
///
/// `changed` distinguishes a genuine state transition from a no-op that returns the
/// current state unchanged — the no-op rows in ADR-142's table are load-bearing, not
/// an absence of behavior, so callers must be able to tell the two apart without
/// re-deriving it from `state`/`terminal_reason` alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub state: AgentState,
    pub terminal_reason: Option<TerminalReason>,
    pub changed: bool,
}

/// A trigger that is not legal from the given state. Never raised for the ADR's
/// explicit no-op rows — those return `Ok(Transition { changed: false, .. })` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IllegalTransition {
    pub from: AgentState,
    pub trigger: Trigger,
}

/// Apply one lifecycle trigger to a record currently in `from` (with `terminal_reason`
/// if `from` is already `Terminal`), per the transition table in ADR-142 §1.
pub fn apply_transition(
    from: AgentState,
    terminal_reason: Option<TerminalReason>,
    trigger: Trigger,
) -> Result<Transition, IllegalTransition> {
    use AgentState::*;
    use Trigger::*;

    match (from, trigger) {
        (Spawned, Dispatch) => Ok(Transition {
            state: Running,
            terminal_reason: None,
            changed: true,
        }),
        (Running, Activity) => Ok(Transition {
            state: Running,
            terminal_reason: None,
            changed: false,
        }),

        (Running, Suspend) => Ok(Transition {
            state: Suspended,
            terminal_reason: None,
            changed: true,
        }),
        (Suspended, Suspend) => Ok(Transition {
            state: Suspended,
            terminal_reason: None,
            changed: false,
        }),

        (Suspended, Resume) => Ok(Transition {
            state: Running,
            terminal_reason: None,
            changed: true,
        }),
        (Running, Resume) => Ok(Transition {
            state: Running,
            terminal_reason: None,
            changed: false,
        }),

        (Spawned, Kill) | (Running, Kill) | (Suspended, Kill) => Ok(Transition {
            state: Terminal,
            terminal_reason: Some(TerminalReason::Killed),
            changed: true,
        }),
        (Terminal, Kill) => Ok(Transition {
            state: Terminal,
            terminal_reason,
            changed: false,
        }),

        (Running, Complete) => Ok(Transition {
            state: Terminal,
            terminal_reason: Some(TerminalReason::Completed),
            changed: true,
        }),
        (Running, Fail) => Ok(Transition {
            state: Terminal,
            terminal_reason: Some(TerminalReason::Failed),
            changed: true,
        }),
        (Running, Abandon) => Ok(Transition {
            state: Terminal,
            terminal_reason: Some(TerminalReason::Abandoned),
            changed: true,
        }),

        (Spawned, HostRestart) | (Running, HostRestart) | (Suspended, HostRestart) => {
            Ok(Transition {
                state: Terminal,
                terminal_reason: Some(TerminalReason::HostRestart),
                changed: true,
            })
        }
        (Terminal, HostRestart) => Ok(Transition {
            state: Terminal,
            terminal_reason,
            changed: false,
        }),

        _ => Err(IllegalTransition { from, trigger }),
    }
}

/// Canonical digest of a spawn's compared argument set (ADR-142 §1, "Persistent process
/// record"): `provider`, `task`, `provider_session_id`, `checkpoint_session_id`, in that
/// key order, absent optionals omitted entirely rather than written as `null`. `idempotency_key`
/// is excluded — it is the replay lookup key, not compared content.
///
/// The key order is built by hand rather than through a `serde_json::Map`/`Value::Object`
/// because `serde_json`'s default map type sorts keys alphabetically (BTreeMap) unless the
/// crate-wide `preserve_order` feature is enabled elsewhere in the dependency graph; hand-
/// ordering keeps this digest's byte layout independent of that feature flag. Digested with
/// BLAKE3 (`khive_types::hash::Hash32`), the same content-hash primitive `khive-db` uses for
/// content-addressed blob refs.
pub fn spawn_fingerprint(
    provider: &str,
    task: &str,
    provider_session_id: Option<&str>,
    checkpoint_session_id: Option<&str>,
) -> String {
    let mut fields: Vec<(&str, &str)> = vec![("provider", provider), ("task", task)];
    if let Some(value) = provider_session_id {
        fields.push(("provider_session_id", value));
    }
    if let Some(value) = checkpoint_session_id {
        fields.push(("checkpoint_session_id", value));
    }

    let mut canonical = String::from("{");
    for (index, (key, value)) in fields.iter().enumerate() {
        if index > 0 {
            canonical.push(',');
        }
        canonical.push_str(&serde_json::to_string(key).expect("string key always serializes"));
        canonical.push(':');
        canonical.push_str(&serde_json::to_string(value).expect("string value always serializes"));
    }
    canonical.push('}');

    Hash32::from_blake3(canonical.as_bytes()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- spawn: (none) -> spawned, via agent.spawn --------------------------------------
    //
    // Record creation, not a transition of an existing record, so it has no
    // `apply_transition` case; `spawn_fingerprint` below covers its comparison logic.

    // -- spawned -> running, via dispatch start (automatic) ------------------------------

    #[test]
    fn dispatch_from_spawned_transitions_to_running() {
        let outcome = apply_transition(AgentState::Spawned, None, Trigger::Dispatch).unwrap();
        assert_eq!(outcome.state, AgentState::Running);
        assert_eq!(outcome.terminal_reason, None);
        assert!(outcome.changed, "spawned -> running is a real transition");
    }

    // -- running -> running, subsequent round (no state change) --------------------------

    #[test]
    fn activity_on_running_is_a_no_op() {
        let outcome = apply_transition(AgentState::Running, None, Trigger::Activity).unwrap();
        assert_eq!(outcome.state, AgentState::Running);
        assert!(
            !outcome.changed,
            "activity on running must be a no-op, not a transition"
        );
    }

    // -- running -> suspended, via agent.suspend; suspended -> suspended no-op -----------

    #[test]
    fn suspend_from_running_transitions_to_suspended() {
        let outcome = apply_transition(AgentState::Running, None, Trigger::Suspend).unwrap();
        assert_eq!(outcome.state, AgentState::Suspended);
        assert!(outcome.changed, "running -> suspended is a real transition");
    }

    #[test]
    fn suspend_on_already_suspended_is_a_no_op_not_an_error() {
        let outcome = apply_transition(AgentState::Suspended, None, Trigger::Suspend).unwrap();
        assert_eq!(outcome.state, AgentState::Suspended);
        assert!(!outcome.changed, "suspend on suspended must be a no-op");
    }

    // -- suspended -> running, via agent.resume; running -> running no-op ----------------

    #[test]
    fn resume_from_suspended_transitions_to_running() {
        let outcome = apply_transition(AgentState::Suspended, None, Trigger::Resume).unwrap();
        assert_eq!(outcome.state, AgentState::Running);
        assert!(outcome.changed, "suspended -> running is a real transition");
    }

    #[test]
    fn resume_on_running_is_a_no_op_not_an_error() {
        let outcome = apply_transition(AgentState::Running, None, Trigger::Resume).unwrap();
        assert_eq!(outcome.state, AgentState::Running);
        assert!(!outcome.changed, "resume on running must be a no-op");
    }

    #[test]
    fn resume_on_spawned_is_an_illegal_transition_error() {
        let err = apply_transition(AgentState::Spawned, None, Trigger::Resume).unwrap_err();
        assert_eq!(
            err,
            IllegalTransition {
                from: AgentState::Spawned,
                trigger: Trigger::Resume,
            }
        );
    }

    #[test]
    fn resume_on_terminal_is_an_illegal_transition_error() {
        let err = apply_transition(
            AgentState::Terminal,
            Some(TerminalReason::Completed),
            Trigger::Resume,
        )
        .unwrap_err();
        assert_eq!(
            err,
            IllegalTransition {
                from: AgentState::Terminal,
                trigger: Trigger::Resume,
            }
        );
    }

    // -- {spawned, running, suspended} -> terminal(killed), via agent.kill ---------------
    // -- terminal -> terminal, kill is a no-op, NEVER an error ---------------------------

    #[test]
    fn kill_from_spawned_transitions_to_terminal_killed() {
        let outcome = apply_transition(AgentState::Spawned, None, Trigger::Kill).unwrap();
        assert_eq!(outcome.state, AgentState::Terminal);
        assert_eq!(outcome.terminal_reason, Some(TerminalReason::Killed));
        assert!(outcome.changed);
    }

    #[test]
    fn kill_from_running_transitions_to_terminal_killed() {
        let outcome = apply_transition(AgentState::Running, None, Trigger::Kill).unwrap();
        assert_eq!(outcome.state, AgentState::Terminal);
        assert_eq!(outcome.terminal_reason, Some(TerminalReason::Killed));
        assert!(outcome.changed);
    }

    #[test]
    fn kill_from_suspended_transitions_to_terminal_killed() {
        let outcome = apply_transition(AgentState::Suspended, None, Trigger::Kill).unwrap();
        assert_eq!(outcome.state, AgentState::Terminal);
        assert_eq!(outcome.terminal_reason, Some(TerminalReason::Killed));
        assert!(outcome.changed);
    }

    #[test]
    fn kill_on_terminal_is_a_no_op_that_returns_current_state_never_an_error() {
        let outcome = apply_transition(
            AgentState::Terminal,
            Some(TerminalReason::Completed),
            Trigger::Kill,
        )
        .expect("kill on terminal must be Ok, never Err");
        assert_eq!(outcome.state, AgentState::Terminal);
        assert_eq!(outcome.terminal_reason, Some(TerminalReason::Completed));
        assert!(!outcome.changed, "kill on terminal must be a no-op");
    }

    // -- running -> terminal(completed), provider returns with no pending tool calls -----

    #[test]
    fn complete_from_running_transitions_to_terminal_completed() {
        let outcome = apply_transition(AgentState::Running, None, Trigger::Complete).unwrap();
        assert_eq!(outcome.state, AgentState::Terminal);
        assert_eq!(outcome.terminal_reason, Some(TerminalReason::Completed));
        assert!(outcome.changed);
    }

    // -- running -> terminal(failed), unrecoverable provider or dispatch error -----------

    #[test]
    fn fail_from_running_transitions_to_terminal_failed() {
        let outcome = apply_transition(AgentState::Running, None, Trigger::Fail).unwrap();
        assert_eq!(outcome.state, AgentState::Terminal);
        assert_eq!(outcome.terminal_reason, Some(TerminalReason::Failed));
        assert!(outcome.changed);
    }

    // -- running -> terminal(abandoned), persistent attachment disconnects ---------------

    #[test]
    fn abandon_from_running_transitions_to_terminal_abandoned() {
        let outcome = apply_transition(AgentState::Running, None, Trigger::Abandon).unwrap();
        assert_eq!(outcome.state, AgentState::Terminal);
        assert_eq!(outcome.terminal_reason, Some(TerminalReason::Abandoned));
        assert!(outcome.changed);
    }

    // -- {spawned, running, suspended} -> terminal(host_restart), boot scan --------------

    #[test]
    fn host_restart_from_spawned_transitions_to_terminal_host_restart() {
        let outcome = apply_transition(AgentState::Spawned, None, Trigger::HostRestart).unwrap();
        assert_eq!(outcome.state, AgentState::Terminal);
        assert_eq!(outcome.terminal_reason, Some(TerminalReason::HostRestart));
        assert!(outcome.changed);
    }

    #[test]
    fn host_restart_from_running_transitions_to_terminal_host_restart() {
        let outcome = apply_transition(AgentState::Running, None, Trigger::HostRestart).unwrap();
        assert_eq!(outcome.state, AgentState::Terminal);
        assert_eq!(outcome.terminal_reason, Some(TerminalReason::HostRestart));
        assert!(outcome.changed);
    }

    #[test]
    fn host_restart_from_suspended_transitions_to_terminal_host_restart() {
        let outcome = apply_transition(AgentState::Suspended, None, Trigger::HostRestart).unwrap();
        assert_eq!(outcome.state, AgentState::Terminal);
        assert_eq!(outcome.terminal_reason, Some(TerminalReason::HostRestart));
        assert!(outcome.changed);
    }

    #[test]
    fn host_restart_on_terminal_is_a_no_op_boot_scan_only_touches_non_terminal() {
        let outcome = apply_transition(
            AgentState::Terminal,
            Some(TerminalReason::Failed),
            Trigger::HostRestart,
        )
        .expect("host_restart on an already-terminal record must be Ok");
        assert_eq!(outcome.state, AgentState::Terminal);
        assert_eq!(outcome.terminal_reason, Some(TerminalReason::Failed));
        assert!(!outcome.changed);
    }

    // -- transitions the table does not license are illegal-transition errors -----------

    #[test]
    fn suspend_on_spawned_is_an_illegal_transition_error() {
        let err = apply_transition(AgentState::Spawned, None, Trigger::Suspend).unwrap_err();
        assert_eq!(
            err,
            IllegalTransition {
                from: AgentState::Spawned,
                trigger: Trigger::Suspend,
            }
        );
    }

    #[test]
    fn suspend_on_terminal_is_an_illegal_transition_error() {
        let err = apply_transition(
            AgentState::Terminal,
            Some(TerminalReason::Completed),
            Trigger::Suspend,
        )
        .unwrap_err();
        assert_eq!(
            err,
            IllegalTransition {
                from: AgentState::Terminal,
                trigger: Trigger::Suspend,
            }
        );
    }

    #[test]
    fn complete_on_suspended_is_an_illegal_transition_error() {
        let err = apply_transition(AgentState::Suspended, None, Trigger::Complete).unwrap_err();
        assert_eq!(
            err,
            IllegalTransition {
                from: AgentState::Suspended,
                trigger: Trigger::Complete,
            }
        );
    }

    // -- spawn_fingerprint -----------------------------------------------------------------

    #[test]
    fn fingerprint_omitting_absent_optional_matches_the_same_call_shape() {
        let a = spawn_fingerprint("anthropic", "do the thing", None, None);
        let b = spawn_fingerprint("anthropic", "do the thing", None, None);
        assert_eq!(a, b, "identical input must digest identically");
    }

    #[test]
    fn fingerprint_absent_optional_differs_from_present_optional() {
        let without = spawn_fingerprint("anthropic", "do the thing", None, None);
        let with_session = spawn_fingerprint("anthropic", "do the thing", Some("sess-1"), None);
        assert_ne!(
            without, with_session,
            "an omitted optional must not digest the same as a present one"
        );
    }

    #[test]
    fn fingerprint_changing_task_changes_the_digest() {
        let original = spawn_fingerprint("anthropic", "do the thing", None, None);
        let changed = spawn_fingerprint("anthropic", "do a different thing", None, None);
        assert_ne!(original, changed, "changing task must change the digest");
    }

    #[test]
    fn fingerprint_is_stable_across_both_optionals_present() {
        let a = spawn_fingerprint("anthropic", "do the thing", Some("sess-1"), Some("chk-1"));
        let b = spawn_fingerprint("anthropic", "do the thing", Some("sess-1"), Some("chk-1"));
        assert_eq!(a, b);
    }

    #[test]
    fn fingerprint_field_order_is_not_confused_with_adjacent_field_content() {
        // provider="a", task="bc" must not collide with provider="ab", task="c" — each
        // field is length-prefixed by JSON string encoding, not naively concatenated.
        let first = spawn_fingerprint("a", "bc", None, None);
        let second = spawn_fingerprint("ab", "c", None, None);
        assert_ne!(first, second);
    }
}
