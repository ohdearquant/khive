//! GTD prepare (kept in kkernel; see the parent module doc for the crate-direction
//! rationale): turns the `khive-pack-gtd` decide step into an atomic plan.

use super::*;

fn require_str<'a>(args: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    args.as_object()
        .and_then(|o| o.get(key))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required field {key:?}"))
}

/// Decide-step wiring for `gtd.transition` (ADR-099 B3 r6 second pass): both
/// this function and `khive-pack-gtd`'s `handle_transition` call
/// `khive_pack_gtd::handlers::duplicate::prepare_transition`, which wraps
/// `prepare_transition` — the ONE place the
/// normalize/validate/secret-gate/load/idempotent-check/lifecycle-guard
/// decision logic lives — and adds the optional duplicate judgment. This
/// function's only job is turning that decision
/// into an `AtomicOpPlan`: the idempotent no-op case produces a guarded
/// mutation-free assertion, and the write case turns the decided patch into a
/// `PlanStatement` via `khive_pack_gtd::handlers::duplicate::transition_statement`
/// — the same DML builder canonical's `atomic_gtd_transition` calls.
pub(super) async fn prepare_gtd_transition(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
) -> anyhow::Result<AtomicOpPlan> {
    let raw_id = require_str(args, "id")?;
    let raw_status = require_str(args, "status")?;
    let note_arg = args
        .as_object()
        .and_then(|o| o.get("note"))
        .and_then(|v| v.as_str());

    let ignore_dependencies = args
        .get("ignore_dependencies")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (decision, duplicate_of) = khive_pack_gtd::handlers::duplicate::prepare_transition(
        runtime,
        token,
        raw_id,
        raw_status,
        note_arg,
        khive_pack_gtd::handlers::DependencyOptions {
            ignore_dependencies,
        },
        args.get("duplicate_of").and_then(Value::as_str),
    )
    .await
    .map_err(anyhow::Error::new)?;

    match decision {
        khive_pack_gtd::handlers::TransitionDecision::NoOp { note, current, .. } => {
            let statement = khive_pack_gtd::handlers::duplicate::noop_assertion_statement(
                &note,
                &current,
                duplicate_of,
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(AtomicOpPlan::GtdTransition(GtdTransitionPlan::new(
                note.id,
                vec![PlanStatement {
                    statement,
                    guard: Some(AffectedRowGuard::exactly(1)),
                }],
                true,
                PostCommitEffect::None,
            )))
        }
        khive_pack_gtd::handlers::TransitionDecision::Write {
            note,
            current,
            target,
            props,
            updated_at,
            transition_note,
        } => {
            let statement = khive_pack_gtd::handlers::duplicate::transition_statement(
                &note,
                &current,
                &target,
                &props,
                updated_at,
                duplicate_of,
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;

            Ok(AtomicOpPlan::GtdTransition(GtdTransitionPlan::new(
                note.id,
                vec![PlanStatement {
                    statement,
                    guard: Some(AffectedRowGuard::exactly(1)),
                }],
                false,
                PostCommitEffect::GtdAudit {
                    task_id: note.id,
                    from_status: current,
                    to_status: target,
                    note: transition_note,
                    namespace: token.namespace().as_str().to_string(),
                },
            )))
        }
    }
}

/// Decide-step wiring for `gtd.complete` — same pattern as
/// [`prepare_gtd_transition`] above: `khive_pack_gtd::handlers::duplicate::
/// prepare_complete` is the single decide step both this function and
/// `handle_complete` call.
pub(super) async fn prepare_gtd_complete(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
) -> anyhow::Result<AtomicOpPlan> {
    let raw_id = require_str(args, "id")?;
    let status_arg = args
        .as_object()
        .and_then(|o| o.get("status"))
        .and_then(|v| v.as_str());
    let result_arg = args
        .as_object()
        .and_then(|o| o.get("result"))
        .and_then(|v| v.as_str());

    let ignore_dependencies = args
        .get("ignore_dependencies")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (decision, duplicate_of) = khive_pack_gtd::handlers::duplicate::prepare_complete(
        runtime,
        token,
        raw_id,
        status_arg,
        result_arg,
        khive_pack_gtd::handlers::DependencyOptions {
            ignore_dependencies,
        },
        args.get("duplicate_of").and_then(Value::as_str),
    )
    .await
    .map_err(anyhow::Error::new)?;

    let statement = khive_pack_gtd::handlers::duplicate::transition_statement(
        &decision.note,
        &decision.current,
        decision.target,
        &decision.props,
        decision.updated_at,
        duplicate_of,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(AtomicOpPlan::GtdComplete(GtdCompletePlan::new(
        decision.note.id,
        vec![PlanStatement {
            statement,
            guard: Some(AffectedRowGuard::exactly(1)),
        }],
        PostCommitEffect::GtdAudit {
            task_id: decision.note.id,
            from_status: decision.current,
            to_status: decision.target.to_string(),
            note: None,
            namespace: token.namespace().as_str().to_string(),
        },
    )))
}
